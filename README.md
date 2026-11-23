# Zenyth Desktop

[Zenyth Web](https://github.com/lowqen/Zenyth-web)(렌주 분석 워크벤치)을 Windows 데스크톱 앱으로 감싼 **Tauri 셸**이다.
화면은 웹과 한 벌이고, 데스크톱이 더하는 것은 셸 쪽 기능이다.

- 공식 Rapfi 네이티브 엔진 동봉 — CPU 에 맞는 빌드(AVX-512 VNNI · AVX-512 · AVX-VNNI · AVX2 · SSE)를 자동으로 고른다
- 원격 엔진(TCP), 파일 연결(기보 형식), 단일 인스턴스, 창 상태 기억, 커스텀 타이틀바
- 자동 업데이트(서명 검증) · 포터블판

2026-11-14 에 Zenyth-web 저장소의 `desktop/` 에서 나왔다. 그 폴더의 커밋 이력은 그대로 따라왔다.
옛 데스크톱(Yixin)과 엔진의 골든 녹취(`baseline/`)는 웹의 엔진 시험이 읽으므로 Zenyth-web 의 `tools/yixin-golden/` 으로 갔다.

## 구조

```
src-tauri/        셸(Rust) — main.rs(창 · 파일 연결 · 업데이트) · engine.rs(네이티브 엔진 · 원격) · settings.rs
                  · library.rs(라이브러리 폴더 소스 — 원자적 쓰기 · 이름 바꾸기 · OS 휴지통)
tests/            셸 설정 ↔ 웹 원본 대조(헤더 · 기보 형식 · 엔진 빌드 id · 업데이트 주소 · 동봉 엔진 판)
web/              (커밋하지 않음) lowqen/Zenyth-web — CI 가 받아 화면을 굽는다
```

**데스크톱 전용 화면 코드도 웹 저장소에 있다** — `src/app/desktop/`(타이틀바 · 셸 다리 · 업데이트),
`src/engine/`의 네이티브 엔진 전송 등. 웹에서 셸 밖이면 꺼지는 코드다. 셸 쪽 짝(명령 이름 · 엔진 빌드 id)이
어긋나지 않는지는 `tests/tauri-config.test.ts` 가 본다.

## 빌드

개발 기기에는 Rust · MSVC 를 깔지 않고 **CI(Windows 러너)가 빌드한다**(`.github/workflows/desktop.yml`).

- `main` 푸시 · PR — 빌드와 시험만, 산출물은 Actions 아티팩트(14일)
- Zenyth-web `main` 의 화면 변경 — 그쪽 `desktop-dispatch.yml` 이 `repository_dispatch`(`web-main`)로 깨운다.
  그 커밋의 화면으로 빌드 · 시험만 한다(시크릿 `DESKTOP_DISPATCH_TOKEN` 은 **Zenyth-web** 쪽에 둔다)
- 수동 실행(`workflow_dispatch`) — `web_ref` 로 화면을 구울 Zenyth-web 의 브랜치 · 태그 · 커밋을 고른다(기본 `main`)
- `desktop-v<버전>` 태그 — 릴리스(설치판 · 서명 · 포터블)와 `desktop-latest` 의 `latest.json` 갱신

버전은 `src-tauri/tauri.conf.json` 과 `src-tauri/Cargo.toml` 을 함께 올린다(시험이 대조한다).

웹 브랜치(PR)의 화면을 셸로 확인하려면 수동 실행에 `web_ref` 를 준다 — 자동으로 깨우는 것은 웹 `main` 뿐이다.
웹 · 데스크톱 분기 규칙과 브라우저 미리 보기(`?desktop`)는 Zenyth-web `docs/engineering-notes.md` §13.

로컬에서 설정 대조만 돌리려면 — Zenyth-web 을 이 저장소 **옆에** 클론해 두고 `web/` 을 정션으로 잇는다
(웹을 고치는 즉시 대조에 반영된다):

```bash
pnpm --dir ../Zenyth-web install
pnpm install
pnpm link-web      # web/ -> ../Zenyth-web (scripts/link-web.ps1, 다른 경로는 -Web)
pnpm test
```

옆 클론이 없으면 예전처럼 `git clone https://github.com/lowqen/Zenyth-web.git web` 으로 받아도 된다.

## 시크릿

| 이름 | 쓰임 |
|---|---|
| `TAURI_SIGNING_PRIVATE_KEY` | 자동 업데이트 서명 개인키(minisign). 공개키는 `tauri.conf.json` `plugins.updater.pubkey` |
| `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` | 그 암호 |

키가 없으면 빌드는 서명 없이 돌고(경고), 릴리스 잡은 멈춘다. **설치된 앱은 이 공개키로만 업데이트를 받으므로
키를 바꾸면 기존 설치는 자동 업데이트가 끊긴다.**

## 자동 업데이트 다리

| 설치된 판 | 업데이트를 읽는 곳 |
|---|---|
| 1.0.3 이하 | `lowqen/Zenyth-web` 릴리스 `desktop-latest` 의 `latest.json` |
| 1.0.4 이상(이 저장소에서 낸 판) | `lowqen/Zenyth-desktop` 릴리스 `desktop-latest` 의 `latest.json` |

이 저장소에서 **처음 릴리스를 낸 뒤 한 번**, 그 `latest.json`(릴리스 잡의 `latest-json` 아티팩트, 또는 이 저장소의
`desktop-latest` 자산)을 Zenyth-web 의 `desktop-latest` 에도 올린다:

```bash
gh release download desktop-latest -R lowqen/Zenyth-desktop -p latest.json -D /tmp/z
gh release upload desktop-latest /tmp/z/latest.json -R lowqen/Zenyth-web --clobber
```

그러면 1.0.3 이하 설치가 이 저장소의 새 판으로 올라오고, 그 판부터는 이 저장소를 읽는다. Zenyth-web 의
`desktop-latest` 와 `desktop-v1.0.3` 릴리스는 옛 설치가 남아 있는 동안 지우지 않는다.

## 라이선스

GPL-3.0-or-later(`LICENSE`). 동봉 엔진 Rapfi 도 GPL-3.0-or-later 이고, 설치 폴더 `engine/SOURCE.txt` 가 대응 소스를
가리킨다. 가중치는 rapfi-networks 의 CC0 자료다.
