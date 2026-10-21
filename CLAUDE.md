# 작업 규칙
- ../pekko-agent, ../rust-pekko 폴더는 읽기 전용이다. 어떤 파일도 수정·생성·삭제하지 마라.
- git push, git remote add는 금지다. 이 프로젝트는 로컬 전용이다.
- API 키는 .env에만 둔다. 코드, 로그, 커밋에 키를 넣지 마라.
- 경로는 상대 경로만 쓴다. Windows와 macOS 모두에서 빌드되어야 한다.
- 한 단계씩 작업하고, 매 단계 cargo build로 확인한다.

# 프로젝트 개요
MSDS PDF를 기능별 에이전트 액터(추출, 성분, 그림문자, 번역)와 총괄 액터로 분석한다.
pekko-actor, pekko-agent-core, -tools, -llm을 라이브러리로 쓰고, 툴 루프는 직접 구현한다.
DB는 docker-compose.yml의 PostgreSQL(포트 5433, DB 이름 msds)을 쓴다.
