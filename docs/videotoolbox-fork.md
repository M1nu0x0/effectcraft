# `videotoolbox` 포크 브랜치

업스트림 릴리즈에 하드웨어 가속(macOS는 VideoToolbox, Windows는 NVENC와 Media Foundation)을 얹은 개인 사용 포크입니다. 공식 릴리즈에 하드웨어 가속이 들어올 때까지 씁니다. 릴리즈 머지, 설치, 실험 절차는 ArtCraft 워크스페이스의 `AGENTS.md`에 있습니다.

## Windows

2026-11-08에 같은 브랜치에 넣었습니다. 이 Mac에서 `cargo check --target x86_64-pc-windows-msvc`까지만 확인했고, Windows에서 빌드하거나 실행한 적은 없습니다.

| 구간 | 코드 | 받는 작업 |
| - | - | - |
| 인코딩 | `crates/export/src/hardware_windows.rs` → `filmcraft_platform::nvenc` | NVIDIA GPU와 드라이버가 있을 때 H.264 High, HEVC Main(8비트, BT.709, 비트레이트 모드) |
| 디코딩 | `crates/media/src/lib.rs`의 `register_hardware_decoders` → `filmcraft_platform::register()` | Media Foundation(D3D11/DXVA)이 받는 스트림 |

- 소프트웨어로 넘기는 작업은 HEVC Main 10, 품질 모드, BT.709가 아닌 출력 색공간, 홀수 크기, NVENC가 세션을 열지 못한 경우입니다. `NVENC declined`가 `warn` 로그로 남습니다.
- B-프레임은 쓰지 않습니다. pts와 dts가 같아 편집 목록이 필요 없습니다.
- 상한 비트레이트는 목표의 1.5배입니다. 소프트웨어 H.264(`H264::new`)와 FilmCraft NVENC 기본값이 같은 값입니다. macOS의 6배(`RATE_CEILING`)는 VideoToolbox 동작 때문이라 가져오지 않았습니다. 실제 비트레이트는 재 보지 않았습니다.
- HEVC 레벨을 지정하면 NVENC에 그대로 넘기고, H.264는 소프트웨어 인코더처럼 무시합니다.
- 드라이버의 `nvEncodeAPI64.dll`(NVENC API 12.1)만 씁니다. CUDA toolkit이나 SDK는 설치하지 않아도 됩니다.
- `register()`는 Windows에서 시작할 때 HEVC 확인용 NVENC 세션을 별도 스레드로 한 번 엽니다(약 0.5초). EC 내보내기는 그 결과를 쓰지 않습니다.
- Main 10과 품질 모드는 FilmCraft를 고치지 않아도 됩니다. `Nvenc::encode_10`이 있어 이 레포에서 10비트 4:2:0을 만들어 넘기면 Main 10이 되고, 품질 모드는 `Config`에 없어 FilmCraft 수정이 필요합니다.

## FilmCraft 수정이 필요한 때

`filmcraft-platform`의 공개 API로 표현되면 이 레포만 고칩니다. 새로운 VideoToolbox 호출이 필요하면 FilmCraft를 고칩니다. 이 워크스페이스는 `unsafe_code = "forbid"`이고, VideoToolbox FFI는 전부 `filmcraft-platform`에 있기 때문입니다.

| 바꾸려는 것 | FilmCraft 수정 | 근거 |
| - | - | - |
| 목표 비트레이트, 상한 배수(`RATE_CEILING`), 키프레임 간격 | 불필요 | `VtConfig` 필드입니다 |
| H.264 Baseline·Main·High, HEVC Main 선택 | 불필요 | `VtProfile`에 있습니다 |
| CBR, VBR 선택 | 불필요 | `VtRate`에 있습니다 |
| 하드웨어로 보낼 작업, 폴백 조건, 로그 | 불필요 | `crates/export/src/hardware_macos.rs`입니다 |
| 인코더 앞의 8비트 4:2:0 변환 | 불필요 | `crates/export/src/encode.rs`입니다 |
| 하드웨어 디코딩 켜기와 끄기, 통계 | 불필요 | `filmcraft_codecs::hw`에 있습니다 |
| 품질 모드, 무손실(`kVTCompressionPropertyKey_Quality`) | 필요 | `VtRate`에 CBR과 VBR만 있습니다 |
| `DataRateLimits` 완전 제거, `VariableBitRate`, VBV 속성 | 필요 | `VtRate::Vbr`이 상한을 항상 설정합니다 |
| 10비트, 4:2:2 인코딩 | 필요 | `VtProfile`이 8비트뿐이고 입력이 `NV12_VIDEO`로 고정입니다 |
| 알파 포함 HEVC, ProRes 하드웨어 인코딩 | 필요 | `VtCodec`에 `H264`와 `Hevc`만 있습니다 |
| VP9, AV1 등의 하드웨어 디코딩 | 필요 | 디코더가 `avcC`와 `hvcC` 스트림만 받습니다 |
| `filmcraft-platform` 내부의 버그 | 필요 | FFI가 그 크레이트에 있습니다 |

결과가 이상하다고 곧바로 FilmCraft 수정으로 가지 않습니다. 먼저 SDK 헤더(`$(xcrun --show-sdk-path)/System/Library/Frameworks/VideoToolbox.framework/Headers/`)와 대조합니다. `DataRateLimits` 상한 때문에 비트레이트가 깎인 문제는 FilmCraft를 고치지 않고 이 레포의 값 하나로 해결됐습니다.

## 수정이 필요할 때의 절차

1. 고정 커밋에 이미 있는지 확인합니다. 루트 `Cargo.toml`의 `filmcraft-*` rev를 쓰고, FilmCraft 클론에서 실행합니다: `git show <rev>:crates/platform/src/videotoolbox_encode.rs`
2. 가져올 수 있는 작업이 있는지 확인합니다: `gh pr list --repo storytold/filmcraft --state open --search videotoolbox`
3. 사용자에게 묻습니다. 포크 생성과 push는 외부에 보이는 동작입니다.
4. 포크를 만듭니다: `gh repo fork storytold/filmcraft --remote=false`
5. 포크를 `FilmCraft-Fork`에 클론하고 원본을 `upstream`으로 추가합니다.
6. 고정 커밋에서 브랜치를 땁니다: `git worktree add ../FilmCraft-VideoToolBox -b videotoolbox <rev>`. FilmCraft `main`에서 따지 않습니다. 2026-11-06 기준 `main`이 고정 커밋보다 1578커밋 앞서 있어 관련 없는 변경이 함께 들어옵니다.
7. 루트 `Cargo.toml`의 `[patch."https://github.com/storytold/filmcraft"]` 구역으로 연결합니다. 업스트림의 rev 줄은 그대로 둡니다. 루트가 쓰는 `filmcraft-*` 크레이트 전부를 포크의 같은 커밋으로 돌립니다. 일부만 돌리면 FilmCraft 타입이 두 벌이 되어 빌드가 깨질 것으로 봅니다. 이 연결 방식은 아직 시험하지 않았습니다.

## 릴리즈 머지와의 관계

- FilmCraft 포크가 없으면 추가 작업이 없습니다.
- FilmCraft 포크가 있으면 머지한 릴리즈의 `filmcraft-*` rev를 확인합니다. 바뀌었으면 새 rev를 FilmCraft 쪽 `videotoolbox`에 먼저 머지하고 `[patch]`의 rev를 갱신합니다.
- `v0.19.0`부터 `v0.21.1`까지 rev는 `09aaf9bf`로 같았습니다. 다만 한 번 움직이면 차이가 클 수 있습니다.

## 현재 상태

2026-11-08 기준 `M1nu0x0/filmcraft`는 없습니다. 이 브랜치는 수정하지 않은 `storytold/filmcraft`의 `d5ead4ba`(`v0.22.0`)를 씁니다.
