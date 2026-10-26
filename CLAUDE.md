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

# 설계 요약
- 액터 5개: 총괄(LLM 호출 없음), 추출, 성분 분석, 그림문자, 번역 에이전트
- 흐름: 업로드 → 총괄에 Analyze → 추출 tell → 완료 시 성분·그림문자·번역에 동시 tell(한글 문서는 번역 생략) → AgentDone 수집 → 상태 저장(분석중/완료/부분 완료/실패), 10분 타임아웃
- 에이전트 액터: receive에서 tokio::spawn으로 내 툴 루프 실행, 끝나면 reply_to.tell(AgentDone)
- 에이전트마다 자기 시스템 프롬프트(순서가 아니라 목표)와 자기 툴만 담은 ToolRegistry를 갖는다
- 메시지에는 doc_id와 섹션 번호만 담는다. 결과는 각 에이전트가 저장 툴로 DB에 직접 저장한다
- 툴: msds_find_headings, msds_save_sections, msds_get_section, msds_save_components(CAS 체크섬 검증, 틀리면 ToolOutput::error), msds_save_hazards, msds_save_translation
  - msds_doc_info {doc_id} → {file_name, page_count, language, products:[{product_no, product_name, section_nos}]}
  - msds_find_headings {doc_id} → {headings:[{page_no, line_no, number, text(120자)}], total_found, truncated, order_ok, backsteps} (최대 200개, 판단은 모델. order_ok=false면 저장하지 말고 보고)
  - msds_save_sections {doc_id, sections:[{product_no=1, product_name?, section_no 1~16(표준 번호), original_no?, title, page_no, line_no}]} → {products:[{product_no, product_name, sections:[{section_no, original_no, title, chars}], missing_sections, warnings}]} (본문은 위치로 잘라 저장, 문서 단위 교체, 2만 자 넘는 섹션 경고)
  - msds_get_section {doc_id, product_no=1, section_no, offset=0} → {product_no, product_name, section_no, original_no, title, text(최대 15000자), offset, total_chars, next_offset|null}
  - msds_save_components {doc_id, product_no=1, product_name?, components:[{product_name?, component, cas_no, percent}]} → {rows:[{component, cas_no, cas_valid}], invalid_cas, notice?} (CAS 틀려도 cas_valid=false로 저장, (doc_id, product_no) 단위 교체)
  - msds_save_hazards {doc_id, product_no=1, h_codes?, classifications?:[{hazard_class(enum), category}], signal_word?, note?} → {status(ok|no_h_codes), pictograms:[{ghs_code, symbol_name, h_codes}], h_codes:[{h_code, sources, evidence, pictograms}], no_pictogram, unknown_codes, unknown_classifications, suppressed_ghs07, warnings} ((doc_id, product_no) 단위 교체)
  - msds_save_translation {doc_id, product_no=1, section_no, chunk_no=0, title_ko, text_ko} → {chars, saved_chunks} ((doc_id, product_no, section_no, chunk_no) 단위 교체)
  - 입력 오류는 ToolOutput::error("오류: 이유"), DB 장애만 Err(ToolError::ExecutionFailed)
- 테이블: msds_documents, msds_pages, msds_sections, msds_components, msds_hazards, msds_translations (CREATE TABLE IF NOT EXISTS, 프로시저·트리거 금지)
- 다중 제품: 한 PDF에 MSDS가 여러 개 이어 붙은 경우(1번 섹션이 여러 번 나옴) product_no를 위치 순서대로 1부터 붙인다. 단일 제품 문서는 product_no 1(기본값). 성분·유해성·번역은 제품마다 따로 저장한다
- 표준 번호 맞추기: section_no는 문서에 찍힌 번호가 아니라 제목의 뜻에 따른 GHS 표준 번호다. 옛 양식(2=성분, 3=유해성)은 성분을 section_no 3·original_no 2로, 유해성을 section_no 2·original_no 3으로 저장한다
- 그림문자는 유해성 섹션(표준 2번)의 H코드와 분류로 판단한다 (이미지 블록 미지원)
  - 문서에 H코드가 없고 분류만 있으면 classifications로 보내 툴이 H코드로 바꾼다. 코드별 출처는 msds_hazards.source(document / classification)
  - H코드도 분류도 없으면(GHS 이전 양식, HMIS·NFPA만 있음) 추측하지 않는다. note에 이유를 적어 h_code NULL 한 행으로 저장하고 status는 no_h_codes
  - GHS07 우선순위는 KOSHA 경고표지 작성 지침(W-14-2022): GHS06이 있으면 GHS07 전부 생략, GHS05(H314·H318)가 있으면 H315·H319의 GHS07 생략, H334가 있으면 H315·H317·H319의 GHS07 생략

# 반드시 지킬 구현 규칙
- 툴 결과는 문자 경계에서 자른다 (바이트 슬라이싱 금지)
- 툴 루프는 최대 10라운드(MSDS_MAX_ROUNDS로 1~20 조정), 다 쓰면 툴 사용 금지 안내를 붙여 한 번 더 호출해 최종 답을 만든다
- tool_use 응답 블록(thinking 포함)은 순서와 내용을 그대로 다시 보낸다
- tool_result는 문자열이다. JSON은 직렬화해서 넣는다
- LlmConfig: timeout_secs 120 이상, temperature None, max_tokens는 에이전트별로 지정(번역 8192 이상)
- CLAUDE_MODEL이 비어 있으면 서버 시작 시 오류로 종료한다
- PDF 추출은 spawn_blocking 안에서 하고, panic은 오류로 바꿔 저장한다
- PDF 텍스트는 좌표 기준으로 줄을 정렬한다(기존 서비스 hybrid_extract.py와 같은 방식)
  - 줄 안은 글자가 아니라 덩어리(그린 순서에서 이어지는 글자들) 단위로 x 정렬한다. 폰트 폭 정보가 틀린 PDF가 있어 글자 단위로 정렬하면 옆 칸과 섞인다
  - 좌표 방식이 실패하거나 결과가 비면 기본 추출(extract_text_from_mem_by_pages)로 대신한다
  - 공백을 뺀 글자 수 합이 200 미만이면 status='failed', error="텍스트가 없는 PDF입니다(스캔본일 수 있음)"
- sqlx는 query! 매크로 대신 sqlx::query() 함수를 쓴다
- 업로드 라우트에 DefaultBodyLimit 20MB
- 샘플 PDF와 업로드 파일은 data/ 아래에만 둔다 (git 제외)

# 에이전트 구조 (src/actors/, 프롬프트는 src/prompts.rs)
- AgentActor<R>: Run { doc_id, reply_to: ActorRef<R> }를 받으면 tokio::spawn으로 작업을 띄우고 바로 돌아온다. R::Message는 From<AgentDone>
- 작업이 끝나면(panic 포함) msds_agent_runs에 한 행(ok, error, rounds, 토큰, 시작·끝 시각)을 남기고 reply_to.tell(AgentDone)
- ToolRegistry는 실행마다 그 에이전트의 툴만 담아 새로 만든다: 추출 = find_headings, save_sections / 성분 = get_section, save_components
- max_tokens: 추출 4096, 성분 8192
- 테스트: cargo run -- agent-test <extract|components|both> <doc_id> (both는 추출 실패 시 성분 생략)

# 사전 검사 (실패하면 Claude를 부르지 않고 ok=false)
- 추출: 제목 후보의 서로 다른 번호가 8개 미만 → "MSDS 문서가 아닌 것으로 보입니다(섹션 제목 부족)", 순서가 뒤로 가면(backsteps) → "섹션 순서가 뒤섞인 문서입니다"
- 성분: msds_sections에 제품이 없으면 → "추출 결과 없음"

# 성공 판정 (모델의 답이 아니라 코드가 DB를 보고 정한다)
- 추출: 저장된 제품마다 표준 2번과 3번 섹션이 있으면 ok
- 성분: 제품마다 msds_components 행이 1개 이상이면 ok

# CAS 원문 원칙
- 성분명, CAS, 함유량은 원문 표기 그대로 저장한다. 모델이 자기 지식으로 CAS를 바꾸거나 채우지 않는다
- invalid_cas가 나오면 원문을 다시 읽어, 잘못 읽은 것만 고친다. 원문 자체가 틀리면(엑셀이 날짜로 바꾼 1964-02-08 등) cas_valid=false로 두고 최종 답에 적는다
