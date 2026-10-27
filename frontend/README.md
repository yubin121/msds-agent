# msds-frontend

MSDS 분석 결과 화면 (Yew, WASM). 메인 크레이트와 따로 빌드한다.

## 실행
```
# 서버 먼저 (저장소 루트)
cargo run

# 화면 (이 폴더)
rustup target add wasm32-unknown-unknown   # 처음 한 번
cargo install trunk                        # 처음 한 번
trunk serve                                # http://localhost:8080
```
- `/api` 요청은 `Trunk.toml`의 proxy로 Worker(8787)를 거쳐 서버(8090)로 간다(CORS 없음). Worker 없이 쓰려면 8090 줄로 바꾼다.
- Tailwind CSS는 trunk가 `Trunk.toml`의 `[tools] tailwindcss` 버전(3.4.17)을 처음 빌드할 때 내려받는다.

## 화면
- `/`: 업로드, 문서 목록(3초마다 갱신, 상태 배지 대기·분석 중·완료·부분 완료·실패·업로드됨)
- `/docs/:id`: 상세(제품 탭, 성분 표와 CAS 체크섬 오류 표시, 그림문자, 신호어, GHS 분류 없음 안내, 섹션별 번역, 에이전트 실행 결과와 실패 이유). 분석 중이면 3초마다 갱신

## 그림문자 이미지 출처
`assets/ghs/GHS01.png` ~ `GHS09.png`는 미국 OSHA(Occupational Safety and Health Administration)의 Hazard Communication 그림문자다.

- 출처: https://www.osha.gov/hazcom/pictograms
- 표기: 각 이미지에 "Photo Credit: OSHA | Copyright: Public Domain" (미국 연방정부 저작물)
- 내려받은 날짜: 2026-10-27
- GHS09만 원본 3660px을 1017px로 축소했다(macOS `sips -Z 1017`, 다른 8개와 같은 크기). 나머지 8개는 원본 그대로다. 화면 크기는 CSS로 조정한다. 서버의 legacy 응답 `image`도 같은 파일을 base64로 넣는다(`src/api_view.rs`가 빌드 때 `include_bytes!`로 읽음)

| 파일 | OSHA 원본 | 그림문자 |
|---|---|---|
| GHS01.png | image3.png | Exploding Bomb |
| GHS02.png | image2.png | Flame |
| GHS03.png | image1.png | Flame Over Circle |
| GHS04.png | image6.png | Gas Cylinder |
| GHS05.png | image5.png | Corrosion |
| GHS06.png | image4.png | Skull and Crossbones |
| GHS07.png | image9.png | Exclamation Mark |
| GHS08.png | image7.png | Health Hazard |
| GHS09.png | image8.png | Environment (원본 3660×3660 약 490KB → 1017×1017 약 97KB로 축소) |

원본 URL: `https://www.osha.gov/sites/default/files/inline-images/imageN.png`

UNECE 그림문자 파일은 쓰지 않는다. UN 웹사이트 이용 조건이 개인·비상업 용도로 한정하고 재배포·2차 저작물을 허용하지 않기 때문이다.
