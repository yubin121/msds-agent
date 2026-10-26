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
  - msds_find_headings {doc_id} → {headings:[{page_no, line_no, number, text(120자), following_lines?}], total_found, truncated, order_ok, backsteps} (최대 200개, 판단은 모델. order_ok=false면 저장하지 말고 보고. 1번 섹션 후보만 아래 빈 줄 아닌 10줄(줄마다 120자)을 following_lines로 준다)
  - msds_save_sections {doc_id, sections:[{product_no=1, product_name?, section_no 1~16(표준 번호), original_no?, title, page_no, line_no}]} → {products:[{product_no, product_name, sections:[{section_no, original_no, title, chars}], missing_sections, warnings}]} (본문은 위치로 잘라 저장, 문서 단위 교체, 2만 자 넘는 섹션 경고)
  - msds_get_section {doc_id, product_no=1, section_no, offset=0, max_chars=15000(1000~15000)} → {product_no, product_name, section_no, original_no, title, text(최대 max_chars자), offset, total_chars, next_offset|null}
  - msds_save_components {doc_id, product_no=1, product_name?, components:[{product_name?, component, cas_no, percent}]} → {rows:[{component, cas_no, cas_valid}], invalid_cas, notice?} (CAS 틀려도 cas_valid=false로 저장, (doc_id, product_no) 단위 교체, 제품명은 msds_sections 이름 우선)
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
- 업로드 크기: 파일 20MB(validate_file), 요청 본문 21MB(DefaultBodyLimit, multipart 헤더 여유)
- 샘플 PDF와 업로드 파일은 data/ 아래에만 둔다 (git 제외)

# 에이전트 구조 (src/actors/, 프롬프트는 src/prompts.rs)
- AgentActor<R>: Run { doc_id, reply_to: ActorRef<R> }를 받으면 tokio::spawn으로 작업을 띄우고 바로 돌아온다. R::Message는 From<AgentDone>
- 작업이 끝나면(panic 포함) msds_agent_runs에 한 행(ok, error, rounds, 토큰, 시작·끝 시각)을 남기고 reply_to.tell(AgentDone)
- ToolRegistry는 실행마다 그 에이전트의 툴만 담아 새로 만든다: 추출 = find_headings, save_sections / 성분 = get_section, save_components / 그림문자 = get_section, save_hazards / 번역은 툴 없음(아래 번역 에이전트 참고)
- max_tokens: 추출 4096, 성분 8192, 그림문자 4096, 번역 8192
- 테스트: cargo run -- agent-test <extract|components|both|hazards|translate> <doc_id> (both는 추출 실패 시 성분 생략)

# 사전 검사 (실패하면 Claude를 부르지 않고 ok=false)
- 추출: 제목 후보의 서로 다른 번호가 8개 미만 → "MSDS 문서가 아닌 것으로 보입니다(섹션 제목 부족)", 순서가 뒤로 가면(backsteps) → "섹션 순서가 뒤섞인 문서입니다"
- 성분: msds_sections에 제품이 없으면 → "추출 결과 없음"

# 성공 판정 (모델의 답이 아니라 코드가 DB를 보고 정한다)
- 추출: 저장된 제품마다 표준 2번과 3번 섹션이 있으면 ok
- 성분: 제품마다 msds_components 행이 1개 이상이면 ok

# CAS 원문 원칙
- 성분명, CAS, 함유량은 원문 표기 그대로 저장한다. 모델이 자기 지식으로 CAS를 바꾸거나 채우지 않는다
- invalid_cas가 나오면 원문을 다시 읽어, 잘못 읽은 것만 고친다. 원문 자체가 틀리면(엑셀이 날짜로 바꾼 1964-02-08 등) cas_valid=false로 두고 최종 답에 적는다

# 제품명 기준
- 제품명은 msds_sections.product_name이 기준이다. 추출 에이전트가 1번 섹션 후보의 following_lines에서 찾는다(Product name, Trade name, 제품명 등. 제품 번호는 제품명이 아님). 못 찾으면 비워 둔다(추측 금지). 단일 제품 문서도 넣는다
- msds_save_components는 msds_sections에 이름이 있으면 그 이름을 복사하고, 없을 때만 입력 값을 쓴다. 성분 에이전트는 제품명을 새로 정하지 않는다

# 다중 CAS와 빈 CAS
- 원문 한 칸에 CAS가 여러 개면 행을 나누지 않는다. cas_no에 쉼표·세미콜론으로 받아 각각 검사하고 "64742-54-7, 64742-65-0"처럼 한 문자열로 저장한다
- cas_valid는 모두 맞을 때만 true, 하나라도 틀리면 false. invalid_cas에는 틀린 번호만 적는다
- 빈 값 목록(대소문자·공백 무시): "", "-", "N/A", "없음", "해당없음", "해당 없음", "자료없음", "None", "Not available", "Not applicable", "N.A.", "NA" → cas_no, cas_valid 모두 NULL
- 비공개 문구(Proprietary, Trade secret, 영업비밀 등)가 있으면 칸 전체를 "비공개"로 보고 쉼표로 나누지 않는다

# 에이전트별 모델
- .env의 CLAUDE_MODEL_EXTRACT, CLAUDE_MODEL_COMPONENTS, CLAUDE_MODEL_HAZARDS, CLAUDE_MODEL_TRANSLATE. 비어 있으면 CLAUDE_MODEL
- AgentActor::from_env가 에이전트마다 자기 모델로 ClaudeClient를 만든다. 시작 로그에 에이전트별 모델 이름을 출력한다(키는 출력 금지)
- msds_agent_runs.model에 실행마다 모델을 저장한다
- 번역 모델은 Sonnet(claude-sonnet-5-5). 근거: 10/26 같은 시험(ATL 전체, ARALDITE 11번)을 두 모델로 비교했을 때 Haiku는 한자 혼입("経피"), 지시문에 대한 혼잣말이 번역문에 섞임, 페이지 꼬리말 삭제가 남았다. Sonnet은 2배쯤 느리지만 이 문제가 없었다

# 업로드 검증 (재시도 대신 입력 검증, 근거: 10/26 소장님 피드백)
- 순서: %PDF- 시그니처 → 크기 20MB 이하 → 공백 제외 텍스트 200자 이상 → 제목 후보의 서로 다른 섹션 번호 8개 이상
- 실패하면 문서 행을 status 'failed', error에 이유로 남기고 400 + {"error": 메시지}. PDF가 아니면 파일을 디스크에 저장하지 않는다
  - 메시지: "PDF 파일이 아닙니다", "파일이 20MB를 넘습니다(..MB)", "텍스트가 없는 PDF입니다(스캔본일 수 있음)", "MSDS 문서가 아닌 것으로 보입니다"
- 검사는 src/validate.rs의 순수 함수(validate_file, validate_text, validate_upload)로만 한다. 다른 런타임(Cloudflare Worker 등)에 같은 규칙으로 옮길 수 있게 DB·IO를 넣지 않는다
- 추출 에이전트의 사전 검사는 안전장치로 남긴다(기준 MIN_DISTINCT_HEADINGS는 headings.rs 공용)
- 저장된 문서 재검사: cargo run -- validate-docs (상태는 바꾸지 않음)

# 그림문자 에이전트 (src/actors/hazards.rs)
- 사전 검사: msds_sections에 제품이 없으면 Claude를 부르지 않고 ok=false("추출 결과 없음")
- 제품마다 표준 2번 섹션을 끝까지 읽고, 문서에 적힌 H코드(묶음 포함)와 신호어를 그대로 보낸다. H코드가 없으면 분류를 classifications로, 둘 다 없으면 h_codes 빈 목록 + note
- 증상 설명, HMIS·NFPA 숫자, 다른 섹션 내용으로 분류를 추측하지 않는다
- 성공 판정: 제품마다 msds_hazards 행이 1개 이상(no_h_codes 행 포함)

# 번역 에이전트 (src/actors/translate.rs)
- language가 ko면 Claude를 부르지 않고 ok=true, 실행 기록 error에 "한글 문서라 번역 생략"
- 툴 루프를 쓰지 않는다. 읽기·저장 순서에 판단이 필요 없는 단계라서다(근거: 툴 루프로 하면 섹션마다 3라운드씩 시스템 프롬프트와 툴 정의가 다시 들어가 입력 토큰이 원문의 약 10배였다. ATL 112,695 → 직접 호출 17,093)
- 코드가 msds_sections에서 본문을 읽어 split_chunks(순수 함수)로 나눈다: 6000자 안에서 마지막 줄바꿈 뒤에서 자르고, 줄바꿈이 없으면 6000자에서 자른다
- 조각마다 Claude를 tools 없이 한 번 부르고(tool_loop::call_once), 응답 텍스트를 msds_save_translation과 같은 저장 로직(save_translation::save_chunk)으로 chunk_no 0부터 저장한다
- (제품, 섹션) 단위로 동시 실행(MSDS_TRANSLATE_CONCURRENCY), 섹션 안의 조각은 차례로. 시작 전에 그 섹션의 기존 번역을 지우고, 조각 하나라도 실패하면 그 섹션 번역을 지워 빠진 섹션으로 잡히게 한다
- title_ko는 LLM에게 받지 않는다. prompts.rs의 SECTION_TITLES_KO로 코드가 chunk 0에 "2. 유해성·위험성"처럼 넣는다. 조각 0의 첫 줄이 섹션 제목(msds_sections.title, 공백·대소문자 무시)과 같으면 번역 요청에서 뺀다(strip_title_line)
- 제품명, CAS, H·P 코드, 화학식, 숫자, 단위, 법규 이름은 원문 그대로. 한국어 부분은 그대로, 요약·생략 금지
- 문체: 지시·주의 문장은 "~하시오", 설명 문장은 "~이다/~한다". 원문 줄바꿈 유지(PDF에서 끊긴 문장은 이어 붙여도 되지만 표의 행은 합치지 않는다). 응답은 번역문만
- 원문을 다시 쓰지 않는다("원문 → 번역" 쌍 형식 금지). 프롬프트 예시에도 "→"를 쓰지 않는다(쌍 형식을 부추길 수 있어서)
- 한글과 라틴 문자만 쓴다(한자·가나 금지). 원문의 화학식·단위·코드는 그대로
- 페이지 머리말·꼬리말은 번역 입력에서 뺀다(근거: 두 모델 모두 페이지 경계에서 꼬리말을 지우거나 문장을 중복했다). msds_sections·msds_pages 원문은 바꾸지 않는다
  - 찾기(순수 함수 find_page_furniture): 페이지마다 맨 위·맨 아래 8줄(빈 줄 제외)을 후보로, 숫자 묶음을 #로 바꾸고 공백을 정리해(normalize_edge_line) 페이지 수의 절반 이상(최소 3페이지)에 나오는 줄
  - 제품마다 따로 센다(product_page_ranges: 그 제품의 첫 섹션 페이지부터 다음 제품의 첫 페이지까지). 다중 제품 문서는 제품마다 머리말이 달라 문서 전체로 세면 절반에 못 미친다(ARALDITE RESIN 28쪽, HARDENER 23쪽 / 51쪽)
  - 8줄인 이유: ARALDITE 머리말이 7줄이라 4줄로는 "Version…", "Print Date" 줄이 빠졌다
  - 빼기(strip_furniture): 정규화한 줄 전체가 같을 때만 뺀다. 조각 0의 제목 줄을 뺀 뒤에 적용한다
- 표 머리글도 번역한다(Health Flammability Reactivity → 건강 인화성 반응성). 표의 행은 합치지 않는다
- 실행 기록 rounds는 Claude 호출 수
- 성공 판정: 대상 섹션 중 원문에 있는 섹션이 모두 저장되면 ok. 빠지면 ok=false, error에 섹션 번호
- 경고(실패 아님, 실행 기록 error에 남김): 사후 검사 결과, stop_reason이 max_tokens인 응답, 섹션 번역 오류

# 번역 사후 검사 (순수 함수 post_check, 모두 실패가 아니라 경고)
- 보존 토큰(preserved_tokens, missing_in_translation): 원문 섹션에서 체크섬이 맞는 CAS 번호, H코드(H200~H499, H360Fd는 H360), P코드(P100~P599), 제품명(원문 섹션에 있을 때)을 뽑아 번역문(조각을 이어 붙인 것)에 그대로 있는지 본다
  - ASCII 단어 경계로 찾아 한글 조사가 붙어도("H301을") 인식한다. EUH 코드는 보지 않는다
- 사후 검사는 머리말·꼬리말을 뺀 원문과 비교한다(번역 입력과 같게). 그래서 제품명 확인은 머리말·꼬리말 밖 원문에 제품명이 있을 때만 한다
- 쌍 형식(added_arrow_lines): "→"가 들어간 줄이 원문보다 1줄이라도 많으면 "원문·번역 쌍 형식 의심"
- 한자·가나(foreign_cjk): 원문에 없는 한자(U+4E00–U+9FFF)·가나(U+3040–U+30FF)가 있으면 경고, 문제 글자를 최대 10개 남긴다
- 라틴 문자 비율(latin_ratio): 경고 아님. 공백 제외 글자 중 라틴 문자(ASCII 영문자, À~ɏ) 비율을 섹션마다 계산해 agent-test에 출력한다(영어가 남은 정도). 성분명·시험 지침 이름처럼 원문 그대로 두는 것도 포함되므로 0이 목표는 아니다

# 환경변수 (개발·비용 조절용, .env.example 참고)
- MSDS_MAX_ROUNDS: 툴 루프 최대 라운드(기본 10, 1~20)
- MSDS_TRANSLATE_CONCURRENCY: 번역 섹션 동시 실행 수(기본 4, 1~16)
- MSDS_TRANSLATE_SECTIONS: 번역할 섹션(예: "1-4,7,8", 비어 있으면 1~16). 형식이 틀리면 번역 에이전트가 ok=false
- MSDS_TRANSLATE_PRODUCTS: 번역할 제품(예: "1", "1-2", 비어 있으면 전부). 형식이 틀리면 번역 에이전트가 ok=false
- 명령 앞에 붙인 환경변수가 .env보다 우선한다(dotenvy는 이미 있는 값을 덮어쓰지 않음). 예: CLAUDE_MODEL_TRANSLATE=claude-sonnet-5-5 cargo run -- agent-test translate <doc_id>
