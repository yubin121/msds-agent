# msds-agent
MSDS PDF를 기능별 에이전트 액터로 분석하는 Rust 프로젝트 (로컬 전용)

## 폴더 배치 (반드시 이 구조)
Agentic_AI/
├── pekko-agent/   yhc007/pekko-agent @ db20850
├── rust-pekko/    daseul312/rust-pekko @ 10c9c42
│   └── coredb/    yhc007/coredb @ a2bba2c
└── msds-agent/    이 프로젝트

## 준비
- Rust (Windows는 Visual Studio Build Tools의 C++ 포함), Docker Desktop
- 의존 저장소는 위 커밋으로 맞춘다: git -C <폴더> checkout <커밋>

## 실행
cp .env.example .env    # CLAUDE_API_KEY, CLAUDE_MODEL 입력
docker compose up -d
cargo run
