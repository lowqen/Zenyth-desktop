//! 라이브러리 폴더 소스(L4) — 웹 `features/library/tauriSource.ts` 의 `FolderBridge` 짝(`src/app/desktop/libraryBridge.ts`).
//!
//! 셸은 파일 창구일 뿐이다. 어느 파일이 라이브러리 항목인지 · 사이드카(`zenyth-library.json`) 해석 · 이름 거르기는
//! 모두 웹이 한다. 셸이 지키는 것:
//!
//! - **이름은 폴더 바로 아래 하나** — 구분자 · `..` · 드라이브 · 대체 스트림(`:`)을 받지 않는다. 폴더는 절대 경로.
//! - **쓰기는 원자적** — 같은 폴더의 `<이름>.zenyth-tmp` 에 조각을 흘려 쓰고, 끝에 `sync_all` → 바꿔 끼우기.
//!   도중에 끊기면 본 파일은 그대로다(임시 파일은 다음 같은 쓰기가 덮는다).
//! - **이름 바꾸기는 덮어쓰지 않는다** — 저널(`.journal`) · 엔진 백업(`.bak*` · `.<숫자>` · `<줄기>_bak<N>.db`)도 함께. 대소문자만 다른
//!   이름은 임시 이름을 거쳐 두 번.
//! - **Windows 공유 위반**(다른 프로그램 · 백신 · 색인기 · 동기화가 쥔 파일) — 짧은 간격으로 몇 번 다시 시도한다.
//!   끝내 안 되면 `busy:` 로 시작하는 오류(웹이 «다른 프로그램이 쓰는 중» 으로 보이고 파일 이름 바꾸기를 미룬다).
//! - **휴지통 = OS 휴지통**(`trash` crate). 되돌리기는 원래 자리 · 이름으로 꺼낸다(같은 이름이 여럿이면 가장 최근 것).

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;
use std::time::{Duration, UNIX_EPOCH};

use serde::Serialize;
use tauri::ipc::{InvokeBody, Request, Response};
use tauri::State;

const MANIFEST: &str = "zenyth-library.json";
const TMP_SUFFIX: &str = ".zenyth-tmp";
const RENAME_SUFFIX: &str = ".zenyth-rename";
const TOKEN_HEADER: &str = "x-lib-token";
/// 한 번에 읽는 최대 — 웹은 8MB 조각으로 묻는다.
const MAX_READ: u64 = 64 << 20;
/// 공유 위반 다시 시도 — 200ms × 10 = 2초.
const RETRIES: u32 = 10;
const RETRY_GAP: Duration = Duration::from_millis(200);

#[derive(Serialize)]
pub struct FileInfo {
    name: String,
    size: u64,
    /// 유닉스 밀리초.
    mtime: u64,
}

struct PendingWrite {
    file: fs::File,
    tmp: PathBuf,
    dest: PathBuf,
}

#[derive(Default)]
pub struct LibraryWrites {
    next: AtomicU32,
    open: Mutex<HashMap<u32, PendingWrite>>,
}

fn dir_of(dir: &str) -> Result<PathBuf, String> {
    let p = PathBuf::from(dir.trim());
    if !p.is_absolute() {
        return Err(format!("폴더 경로가 절대 경로가 아닙니다: {dir}"));
    }
    if !p.is_dir() {
        return Err(format!("폴더가 없습니다: {}", p.display()));
    }
    Ok(p)
}

/// 폴더 바로 아래의 이름 하나인가.
fn check_name(name: &str) -> Result<(), String> {
    let bad = name.is_empty()
        || name == "."
        || name == ".."
        || name.contains(['/', '\\', ':', '\0'])
        || Path::new(name).components().count() != 1
        || !matches!(Path::new(name).components().next(), Some(Component::Normal(_)));
    if bad {
        Err(format!("쓸 수 없는 파일 이름입니다: {name}"))
    } else {
        Ok(())
    }
}

fn path_of(dir: &str, name: &str) -> Result<PathBuf, String> {
    check_name(name)?;
    Ok(dir_of(dir)?.join(name))
}

fn mtime_ms(meta: &fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Windows 의 공유 위반(32) · 잠금 위반(33) · 접근 거부(5, 지우는 중인 파일 따위).
fn is_busy(e: &std::io::Error) -> bool {
    matches!(e.raw_os_error(), Some(32) | Some(33) | Some(5))
}

/// 공유 위반이면 몇 번 다시 시도한다. 끝내 안 되면 `busy:` 로 시작하는 오류.
fn retry<T>(what: &Path, mut f: impl FnMut() -> std::io::Result<T>) -> Result<T, String> {
    let mut last = None;
    for i in 0..RETRIES {
        match f() {
            Ok(v) => return Ok(v),
            Err(e) if is_busy(&e) => {
                last = Some(e);
                if i + 1 < RETRIES {
                    std::thread::sleep(RETRY_GAP);
                }
            }
            Err(e) => return Err(format!("{}: {e}", what.display())),
        }
    }
    Err(format!(
        "busy: {}: {}",
        what.display(),
        last.map(|e| e.to_string()).unwrap_or_default()
    ))
}

fn info_of(name: String, meta: &fs::Metadata) -> FileInfo {
    FileInfo {
        name,
        size: meta.len(),
        mtime: mtime_ms(meta),
    }
}

/// 엔진이 본 파일 곁에 두는 짝의 꼬리 — 웹 `source.ts` 의 `companionSuffix` 와 같은 규칙.
/// 저널 · 백업은 `.journal` 처럼 본 이름 뒤에 붙고, 엔진 번호 백업(DB1 `makeBackupFilePath`,
/// `<줄기>_bak<N><확장자>`)은 `_bak1.db` 처럼 줄기에 붙는다 — 붙이는 쪽은 `companion_name`.
fn companion_suffix(base: &str, name: &str) -> Option<String> {
    if name.len() > base.len()
        && name.is_char_boundary(base.len())
        && name[..base.len()].to_lowercase() == base.to_lowercase()
    {
        let rest = &name[base.len()..];
        if let Some(tail) = rest.strip_prefix('.') {
            let lower = tail.to_ascii_lowercase();
            let digits = |s: &str| s.chars().all(|c| c.is_ascii_digit());
            let ok = lower == "journal"
                || (!lower.is_empty() && digits(&lower))
                || lower.strip_prefix("bak").is_some_and(digits)
                || lower.strip_prefix("backup").is_some_and(digits);
            if ok {
                return Some(rest.to_owned());
            }
        }
    }
    backup_tail(base, name)
}

/// 엔진 번호 백업의 꼬리(`_bak1.db`) — 본 파일이 `.db` 일 때만.
fn backup_tail(base: &str, name: &str) -> Option<String> {
    let stem = strip_db(base)?;
    if name.len() <= stem.len() || !name.is_char_boundary(stem.len()) {
        return None;
    }
    if name[..stem.len()].to_lowercase() != stem.to_lowercase() {
        return None;
    }
    let rest = &name[stem.len()..];
    let lower = rest.to_ascii_lowercase();
    let n = lower.strip_prefix("_bak")?.strip_suffix(".db")?;
    (!n.is_empty() && n.chars().all(|c| c.is_ascii_digit())).then(|| rest.to_owned())
}

/// `.db` 를 뗀 줄기(대소문자 무시).
fn strip_db(name: &str) -> Option<&str> {
    let cut = name.len().checked_sub(3)?;
    (name.is_char_boundary(cut) && name[cut..].eq_ignore_ascii_case(".db")).then(|| &name[..cut])
}

/// 짝 꼬리를 본 이름에 붙인다 — 웹 `withTail`.
fn companion_name(base: &str, tail: &str) -> String {
    if tail.starts_with('_') {
        format!("{}{tail}", strip_db(base).unwrap_or(base))
    } else {
        format!("{base}{tail}")
    }
}

fn companions(dir: &Path, base: &str) -> Vec<String> {
    let Ok(rd) = fs::read_dir(dir) else {
        return Vec::new();
    };
    rd.filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .filter_map(|e| companion_suffix(base, &e.file_name().to_string_lossy()))
        .collect()
}

#[tauri::command]
pub async fn lib_exists(dir: String) -> bool {
    dir_of(&dir).is_ok()
}

/// 폴더 바로 아래의 파일 — 숨은 파일 · 사이드카 · 임시 파일은 뺀다.
#[tauri::command]
pub async fn lib_list(dir: String) -> Result<Vec<FileInfo>, String> {
    let d = dir_of(&dir)?;
    let rd = fs::read_dir(&d).map_err(|e| format!("{}: {e}", d.display()))?;
    let mut out = Vec::new();
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with('.')
            || name == MANIFEST
            || name.ends_with(TMP_SUFFIX)
            || name.ends_with(RENAME_SUFFIX)
        {
            continue;
        }
        let Ok(meta) = e.metadata() else { continue };
        if meta.is_file() {
            out.push(info_of(name, &meta));
        }
    }
    Ok(out)
}

#[tauri::command]
pub async fn lib_stat(dir: String, name: String) -> Result<Option<FileInfo>, String> {
    let p = path_of(&dir, &name)?;
    match fs::metadata(&p) {
        Ok(m) if m.is_file() => Ok(Some(info_of(name, &m))),
        Ok(_) => Ok(None),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{}: {e}", p.display())),
    }
}

/// `offset` 부터 `length` 바이트(끝이면 짧게) — 날것으로 돌려준다(JSON 배열이 아니라 ArrayBuffer).
#[tauri::command]
pub async fn lib_read(dir: String, name: String, offset: u64, length: u64) -> Result<Response, String> {
    let p = path_of(&dir, &name)?;
    let len = length.min(MAX_READ);
    let mut f = retry(&p, || fs::File::open(&p))?;
    f.seek(SeekFrom::Start(offset)).map_err(|e| format!("{}: {e}", p.display()))?;
    let mut buf = Vec::with_capacity(len as usize);
    f.take(len)
        .read_to_end(&mut buf)
        .map_err(|e| format!("{}: {e}", p.display()))?;
    Ok(Response::new(buf))
}

#[tauri::command]
pub async fn lib_write_begin(
    dir: String,
    name: String,
    writes: State<'_, LibraryWrites>,
) -> Result<u32, String> {
    let dest = path_of(&dir, &name)?;
    let tmp = dest.with_file_name(format!("{name}{TMP_SUFFIX}"));
    let file = fs::File::create(&tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
    let token = writes.next.fetch_add(1, Ordering::Relaxed) + 1;
    writes
        .open
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(token, PendingWrite { file, tmp, dest });
    Ok(token)
}

/// 조각 하나 — 몸통은 날 바이트, 토큰은 머리글.
#[tauri::command]
pub async fn lib_write_chunk(request: Request<'_>, writes: State<'_, LibraryWrites>) -> Result<(), String> {
    let InvokeBody::Raw(bytes) = request.body() else {
        return Err("조각은 날 바이트로 보내야 합니다".into());
    };
    let token: u32 = request
        .headers()
        .get(TOKEN_HEADER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse().ok())
        .ok_or("쓰기 토큰이 없습니다")?;
    let mut open = writes.open.lock().unwrap_or_else(|e| e.into_inner());
    let w = open.get_mut(&token).ok_or("끝났거나 없는 쓰기입니다")?;
    w.file
        .write_all(bytes)
        .map_err(|e| format!("{}: {e}", w.tmp.display()))
}

/// 끝 — 디스크에 내리고 본 이름으로 바꿔 끼운다(본 파일을 덮는 것이 뜻이다).
#[tauri::command]
pub async fn lib_write_commit(token: u32, writes: State<'_, LibraryWrites>) -> Result<(), String> {
    let w = writes
        .open
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&token)
        .ok_or("끝났거나 없는 쓰기입니다")?;
    let PendingWrite { file, tmp, dest } = w;
    let res = file
        .sync_all()
        .map_err(|e| format!("{}: {e}", tmp.display()))
        .and_then(|_| {
            drop(file);
            retry(&dest, || fs::rename(&tmp, &dest))
        });
    if res.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    res
}

#[tauri::command]
pub async fn lib_write_abort(token: u32, writes: State<'_, LibraryWrites>) -> Result<(), String> {
    if let Some(w) = writes.open.lock().unwrap_or_else(|e| e.into_inner()).remove(&token) {
        drop(w.file);
        let _ = fs::remove_file(&w.tmp);
    }
    Ok(())
}

#[tauri::command]
pub async fn lib_remove(dir: String, name: String) -> Result<(), String> {
    let p = path_of(&dir, &name)?;
    retry(&p, || match fs::remove_file(&p) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        r => r,
    })
}

/// 덮어쓰지 않는 옮기기 하나 — 있으면 실패, 공유 위반은 다시 시도.
fn move_no_replace(from: &Path, to: &Path) -> Result<(), String> {
    if to.exists() {
        return Err(format!("exists: {}", to.display()));
    }
    retry(from, || fs::rename(from, to))
}

#[tauri::command]
pub async fn lib_rename(dir: String, from: String, to: String) -> Result<(), String> {
    check_name(&from)?;
    check_name(&to)?;
    let d = dir_of(&dir)?;
    let src = d.join(&from);
    if !src.is_file() {
        return Err(format!("missing: {}", src.display()));
    }
    let case_only = from != to && from.to_lowercase() == to.to_lowercase();
    let tails = companions(&d, &from);
    if !case_only {
        for t in std::iter::once(String::new()).chain(tails.iter().cloned()) {
            let p = d.join(companion_name(&to, &t));
            if p.exists() {
                return Err(format!("exists: {}", p.display()));
            }
        }
    }
    let mut done: Vec<(PathBuf, PathBuf)> = Vec::new();
    for t in std::iter::once(String::new()).chain(tails.into_iter()) {
        let a = d.join(companion_name(&from, &t));
        let b = d.join(companion_name(&to, &t));
        let step = if case_only {
            let mid = d.join(format!("{}{RENAME_SUFFIX}", companion_name(&from, &t)));
            move_no_replace(&a, &mid).and_then(|_| {
                move_no_replace(&mid, &b).inspect_err(|_| {
                    let _ = fs::rename(&mid, &a);
                })
            })
        } else {
            move_no_replace(&a, &b)
        };
        if let Err(e) = step {
            // 반쯤 옮긴 짝을 되돌린다 — 본 파일과 저널이 다른 이름에 갈라져 남지 않게.
            for (a, b) in done.iter().rev() {
                let _ = fs::rename(b, a);
            }
            return Err(e);
        }
        done.push((a, b));
    }
    Ok(())
}

#[tauri::command]
pub async fn lib_trash(dir: String, name: String) -> Result<(), String> {
    let d = dir_of(&dir)?;
    let p = path_of(&dir, &name)?;
    let mut paths = vec![p.clone()];
    paths.extend(companions(&d, &name).into_iter().map(|t| d.join(companion_name(&name, &t))));
    for q in paths {
        // 셸 휴지통 API 의 오류는 OS 코드로 오지 않는다 — 다른 프로그램이 쥔 순간을 넘기려 몇 번만 다시 시도한다.
        let mut last = String::new();
        let mut ok = false;
        for i in 0..3 {
            match trash::delete(&q) {
                Ok(()) => {
                    ok = true;
                    break;
                }
                Err(e) => {
                    last = e.to_string();
                    if i < 2 {
                        std::thread::sleep(RETRY_GAP);
                    }
                }
            }
        }
        if !ok {
            return Err(format!("busy: {}: {last}", q.display()));
        }
    }
    Ok(())
}

/// 경로 비교용 — `\\?\` 앞붙이 · 끝 구분자 · 대소문자를 걷는다.
fn norm(p: &Path) -> String {
    let s = p.to_string_lossy().replace('/', "\\");
    let s = s.strip_prefix(r"\\?\").unwrap_or(&s);
    s.trim_end_matches('\\').to_lowercase()
}

/// OS 휴지통에서 이 폴더의 `name` 과 그 짝 — 같은 이름이 여럿이면 가장 최근에 지운 것.
fn trashed(dir: &Path, name: &str) -> Result<Vec<trash::TrashItem>, String> {
    let want = norm(dir);
    let items = trash::os_limited::list().map_err(|e| e.to_string())?;
    let mine: Vec<trash::TrashItem> = items
        .into_iter()
        .filter(|i| norm(&i.original_parent) == want)
        .collect();
    let base_of = |i: &trash::TrashItem| Path::new(&i.name).to_string_lossy().into_owned();
    let Some(newest) = mine
        .iter()
        .filter(|i| base_of(i).to_lowercase() == name.to_lowercase())
        .map(|i| i.time_deleted)
        .max()
    else {
        return Ok(Vec::new());
    };
    // 본 파일과 같은 때(±5초) 지운 짝만.
    Ok(mine
        .into_iter()
        .filter(|i| {
            let n = base_of(i);
            let close = (i.time_deleted - newest).abs() <= 5;
            close && (n.to_lowercase() == name.to_lowercase() || companion_suffix(name, &n).is_some())
        })
        .collect())
}

#[tauri::command]
pub async fn lib_restore(dir: String, name: String) -> Result<(), String> {
    check_name(&name)?;
    let d = dir_of(&dir)?;
    let items = trashed(&d, &name)?;
    if items.is_empty() {
        return Err(format!("휴지통에 없습니다: {name}"));
    }
    if d.join(&name).exists() {
        return Err(format!("exists: {}", d.join(&name).display()));
    }
    trash::os_limited::restore_all(items).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn lib_purge(dir: String, name: String) -> Result<(), String> {
    check_name(&name)?;
    let d = dir_of(&dir)?;
    let items = trashed(&d, &name)?;
    if items.is_empty() {
        return Ok(());
    }
    trash::os_limited::purge_all(items).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_stay_in_folder() {
        for bad in ["", ".", "..", "a/b", "a\\b", "c:x", "..\\x", "x\0"] {
            assert!(check_name(bad).is_err(), "{bad:?}");
        }
        for ok in ["a.db", "화월 연구.db", "a.db.journal", ".zenyth-x", "zenyth-library.json"] {
            assert!(check_name(ok).is_ok(), "{ok:?}");
        }
    }

    #[test]
    fn companions_match_web() {
        assert_eq!(companion_suffix("a.db", "a.db.journal").as_deref(), Some(".journal"));
        assert_eq!(companion_suffix("a.db", "A.DB.bak1").as_deref(), Some(".bak1"));
        assert_eq!(companion_suffix("a.db", "a.db.2").as_deref(), Some(".2"));
        assert_eq!(companion_suffix("a.db", "a.db.tmp"), None);
        assert_eq!(companion_suffix("a.db", "a.dbx"), None);
        assert_eq!(companion_suffix("a.db", "a.db"), None);
        // 엔진 번호 백업(DB1): <줄기>_bak<N>.db
        assert_eq!(companion_suffix("a.db", "a_bak1.db").as_deref(), Some("_bak1.db"));
        assert_eq!(companion_suffix("연구.db", "연구_BAK12.db").as_deref(), Some("_BAK12.db"));
        assert_eq!(companion_suffix("a.db", "ab_bak1.db"), None);
        assert_eq!(companion_suffix("a.db", "a_bak.db"), None);
        assert_eq!(companion_suffix("a.txt", "a_bak1.txt"), None);
        assert_eq!(companion_name("b.db", "_bak1.db"), "b_bak1.db");
        assert_eq!(companion_name("b.db", ".journal"), "b.db.journal");
    }

    #[test]
    fn rename_moves_companions_without_replacing() {
        let d = std::env::temp_dir().join(format!("zenyth-lib-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("a.db"), b"base").unwrap();
        fs::write(d.join("a.db.journal"), b"j").unwrap();
        fs::write(d.join("a_bak1.db"), b"k").unwrap();
        fs::write(d.join("taken.db"), b"x").unwrap();
        let dir = d.to_string_lossy().into_owned();
        let rt = tauri::async_runtime::block_on(lib_rename(dir.clone(), "a.db".into(), "taken.db".into()));
        assert!(rt.unwrap_err().starts_with("exists"));
        tauri::async_runtime::block_on(lib_rename(dir.clone(), "a.db".into(), "b.db".into())).unwrap();
        assert_eq!(fs::read(d.join("b.db")).unwrap(), b"base");
        assert_eq!(fs::read(d.join("b.db.journal")).unwrap(), b"j");
        assert_eq!(fs::read(d.join("b_bak1.db")).unwrap(), b"k");
        assert!(!d.join("a.db").exists());
        assert!(!d.join("a_bak1.db").exists());
        tauri::async_runtime::block_on(lib_rename(dir.clone(), "b.db".into(), "B.db".into())).unwrap();
        let names: Vec<String> = fs::read_dir(&d)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(names.contains(&"B.db".to_owned()), "{names:?}");
        assert!(names.contains(&"B.db.journal".to_owned()), "{names:?}");
        assert!(names.contains(&"B_bak1.db".to_owned()), "{names:?}");
        let _ = fs::remove_dir_all(&d);
    }
}
