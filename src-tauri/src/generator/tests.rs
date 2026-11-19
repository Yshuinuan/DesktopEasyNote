//! 生成引擎测试（R12）：临时目录 + 注入时钟/配置/HTTP 客户端 + 本地 mock HTTP 服务。
//!
//! 原则：
//! - 不写用户真实 `data/`、`output/`；每个测试用独立临时目录并在 Drop 时删除。
//! - 不使用真实 key；mock 的 key 是假值。
//! - `include_str!` 内嵌 `docs/旁白提示词-v3.0.md` **只在本测试模块**使用，
//!   比对 ① 系统提示词 / ② 用户消息模板 / ③ 重试追加段逐字一致（CRLF 归一）。

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use chrono::{DateTime, Local, TimeZone};

use super::ai::{ChatClient, ChatError, ChatRequest, ReqwestChatClient};
use super::schedule::{self, Clock};
use super::{ai, output, prompt, EngineState, Outcome, RetryPolicy, SkipReason, Trigger, TOAST_MESSAGE};
use super::{ConfigSource, Engine, GenConfig, GenPaths, LogLevel, LogSink, Notifier};

/// 假 key（不是真实 key）。
const FAKE_KEY: &str = "sk-fake-test-key-not-real";

fn config_with_key() -> GenConfig {
    GenConfig {
        api_key: Some(FAKE_KEY.to_string()),
        base_url: "http://127.0.0.1:9".to_string(),
        model: "deepseek-chat".to_string(),
    }
}

fn config_without_key() -> GenConfig {
    GenConfig {
        api_key: None,
        base_url: "http://127.0.0.1:9".to_string(),
        model: "deepseek-chat".to_string(),
    }
}

fn local_time(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> DateTime<Local> {
    Local
        .with_ymd_and_hms(year, month, day, hour, minute, 0)
        .single()
        .expect("测试用本地时间应存在")
}

// ---------------------------------------------------------------------------
// 临时目录
// ---------------------------------------------------------------------------

struct TestPaths {
    root: PathBuf,
}

impl TestPaths {
    fn new(label: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let index = COUNTER.fetch_add(1, Ordering::SeqCst);
        let root = std::env::temp_dir().join(format!(
            "desktop-narrator-generator-test-{}-{}-{}",
            std::process::id(),
            index,
            label
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("data")).expect("创建临时 data 目录");
        std::fs::create_dir_all(root.join("output")).expect("创建临时 output 目录");
        Self { root }
    }

    fn gen_paths(&self) -> GenPaths {
        GenPaths {
            data_dir: self.root.join("data"),
            output_dir: self.root.join("output"),
        }
    }

    fn data_dir(&self) -> PathBuf {
        self.root.join("data")
    }

    fn output_dir(&self) -> PathBuf {
        self.root.join("output")
    }

    fn data_path(&self, day: &str) -> PathBuf {
        self.data_dir().join(format!("{day}.md"))
    }

    fn output_path(&self, day: &str) -> PathBuf {
        self.output_dir().join(format!("{day}.md"))
    }

    fn write_data(&self, day: &str, content: &str) {
        std::fs::write(self.data_path(day), content).expect("写入临时记录文件");
    }

    fn output_text(&self, day: &str) -> Option<String> {
        std::fs::read_to_string(self.output_path(day)).ok()
    }
}

impl Drop for TestPaths {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

// ---------------------------------------------------------------------------
// 注入依赖：时钟 / 日志 / 提示 / 配置 / HTTP
// ---------------------------------------------------------------------------

struct ManualClock {
    now: Mutex<DateTime<Local>>,
    sleeps: Mutex<Vec<Duration>>,
}

impl ManualClock {
    fn at(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> Arc<Self> {
        Arc::new(Self {
            now: Mutex::new(local_time(year, month, day, hour, minute)),
            sleeps: Mutex::new(Vec::new()),
        })
    }

    fn set(&self, year: i32, month: u32, day: u32, hour: u32, minute: u32) {
        *self.now.lock().expect("时钟锁") = local_time(year, month, day, hour, minute);
    }

    fn sleep_count(&self) -> usize {
        self.sleeps.lock().expect("睡眠记录锁").len()
    }
}

impl Clock for ManualClock {
    fn now(&self) -> DateTime<Local> {
        *self.now.lock().expect("时钟锁")
    }

    fn sleep(&self, duration: Duration) {
        self.sleeps.lock().expect("睡眠记录锁").push(duration);
    }
}

#[derive(Default)]
struct RecordingLog {
    entries: Mutex<Vec<(LogLevel, String)>>,
}

impl RecordingLog {
    fn messages(&self) -> Vec<String> {
        self.entries
            .lock()
            .expect("日志锁")
            .iter()
            .map(|(_level, message)| message.clone())
            .collect()
    }

    fn joined(&self) -> String {
        self.messages().join("\n")
    }

    fn count(&self, needle: &str) -> usize {
        self.messages()
            .iter()
            .filter(|line| line.contains(needle))
            .count()
    }

    /// 指定级别的日志条数（R15 断言「没有 WARN」用）。
    fn count_level(&self, level: LogLevel) -> usize {
        self.entries
            .lock()
            .expect("日志锁")
            .iter()
            .filter(|(entry_level, _message)| *entry_level == level)
            .count()
    }

    /// 指定级别里是否有一条包含 `needle`。
    fn level_contains(&self, level: LogLevel, needle: &str) -> bool {
        self.entries
            .lock()
            .expect("日志锁")
            .iter()
            .any(|(entry_level, message)| *entry_level == level && message.contains(needle))
    }
}

impl LogSink for RecordingLog {
    fn write(&self, level: LogLevel, message: &str) {
        self.entries
            .lock()
            .expect("日志锁")
            .push((level, message.to_string()));
    }
}

#[derive(Default)]
struct RecordingNotifier {
    messages: Mutex<Vec<String>>,
}

impl RecordingNotifier {
    fn messages(&self) -> Vec<String> {
        self.messages.lock().expect("提示记录锁").clone()
    }
}

impl Notifier for RecordingNotifier {
    fn notify(&self, message: &str) {
        self.messages
            .lock()
            .expect("提示记录锁")
            .push(message.to_string());
    }
}

struct StaticConfig {
    config: Mutex<GenConfig>,
}

impl StaticConfig {
    fn new(config: GenConfig) -> Arc<Self> {
        Arc::new(Self {
            config: Mutex::new(config),
        })
    }

    fn set_key(&self, key: Option<String>) {
        self.config.lock().expect("配置锁").api_key = key;
    }
}

impl ConfigSource for StaticConfig {
    fn load(&self) -> GenConfig {
        self.config.lock().expect("配置锁").clone()
    }
}

struct FakeClient {
    responses: Mutex<VecDeque<Result<String, ChatError>>>,
    calls: Mutex<Vec<ChatRequest>>,
}

impl FakeClient {
    fn new(responses: Vec<Result<String, ChatError>>) -> Arc<Self> {
        Arc::new(Self {
            responses: Mutex::new(responses.into()),
            calls: Mutex::new(Vec::new()),
        })
    }

    fn call_count(&self) -> usize {
        self.calls.lock().expect("调用记录锁").len()
    }

    fn requests(&self) -> Vec<ChatRequest> {
        self.calls.lock().expect("调用记录锁").clone()
    }
}

impl ChatClient for FakeClient {
    fn complete(&self, request: &ChatRequest) -> Result<String, ChatError> {
        self.calls
            .lock()
            .expect("调用记录锁")
            .push(request.clone());
        self.responses
            .lock()
            .expect("响应队列锁")
            .pop_front()
            .unwrap_or_else(|| Err(ChatError::Shape("测试未准备更多响应".to_string())))
    }
}

// ---------------------------------------------------------------------------
// 本地 mock HTTP 服务（TcpListener 自建，不引新依赖）
// ---------------------------------------------------------------------------

struct MockResponse {
    status: u16,
    body: String,
}

impl MockResponse {
    fn json(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            body: body.into(),
        }
    }
}

struct MockServer {
    addr: SocketAddr,
    shutdown: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    heads: Arc<Mutex<Vec<String>>>,
    bodies: Arc<Mutex<Vec<String>>>,
}

impl MockServer {
    fn start(responses: Vec<MockResponse>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("绑定本地端口");
        listener.set_nonblocking(true).expect("设置非阻塞");
        let addr = listener.local_addr().expect("读取本地地址");
        let shutdown = Arc::new(AtomicBool::new(false));
        let queue = Arc::new(Mutex::new(VecDeque::from(responses)));
        let heads = Arc::new(Mutex::new(Vec::new()));
        let bodies = Arc::new(Mutex::new(Vec::new()));

        let thread_shutdown = shutdown.clone();
        let thread_heads = heads.clone();
        let thread_bodies = bodies.clone();
        let handle = std::thread::spawn(move || {
            while !thread_shutdown.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _peer)) => {
                        handle_connection(stream, &queue, &thread_heads, &thread_bodies);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_error) => break,
                }
            }
        });

        Self {
            addr,
            shutdown,
            handle: Some(handle),
            heads,
            bodies,
        }
    }

    fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn request_count(&self) -> usize {
        self.heads.lock().expect("请求头锁").len()
    }

    fn request_head(&self, index: usize) -> String {
        self.heads.lock().expect("请求头锁")[index].clone()
    }

    fn request_body(&self, index: usize) -> String {
        self.bodies.lock().expect("请求体锁")[index].clone()
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn handle_connection(
    mut stream: TcpStream,
    queue: &Arc<Mutex<VecDeque<MockResponse>>>,
    heads: &Arc<Mutex<Vec<String>>>,
    bodies: &Arc<Mutex<Vec<String>>>,
) {
    // Windows 上 accepted socket 可能继承监听端的非阻塞模式，这里显式改回阻塞 + 读超时，
    // 否则偶发 WouldBlock 会让客户端收到连接被重置（表现为 reqwest "error sending request"）。
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let Some((head, body)) = read_request(&mut stream) else {
        return;
    };
    heads.lock().expect("请求头锁").push(head);
    bodies.lock().expect("请求体锁").push(body);

    let response = queue
        .lock()
        .expect("响应队列锁")
        .pop_front()
        .unwrap_or(MockResponse {
            status: 500,
            body: "{\"error\":\"mock 响应已用尽\"}".to_string(),
        });

    let reason = match response.status {
        200 => "OK",
        401 => "Unauthorized",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        _ => "Status",
    };
    let payload = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        response.status,
        reason,
        response.body.as_bytes().len(),
        response.body
    );
    let _ = stream.write_all(payload.as_bytes());
    let _ = stream.flush();
}

fn read_request(stream: &mut TcpStream) -> Option<(String, String)> {
    let mut buffer: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];

    loop {
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);

        if let Some(position) = find_header_end(&buffer) {
            let head = String::from_utf8_lossy(&buffer[..position]).to_string();
            let content_length = parse_content_length(&head);
            let mut body: Vec<u8> = buffer[position + 4..].to_vec();
            while body.len() < content_length {
                let read = stream.read(&mut chunk).ok()?;
                if read == 0 {
                    break;
                }
                body.extend_from_slice(&chunk[..read]);
            }
            return Some((head, String::from_utf8_lossy(&body).to_string()));
        }
    }
}

fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer
        .windows(4)
        .position(|window| window == b"\r\n\r\n".as_slice())
}

fn parse_content_length(head: &str) -> usize {
    head.lines()
        .find_map(|line| {
            let lower = line.to_ascii_lowercase();
            lower
                .strip_prefix("content-length:")
                .map(|value| value.trim().to_string())
        })
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// 测试夹具
// ---------------------------------------------------------------------------

struct Harness {
    paths: TestPaths,
    clock: Arc<ManualClock>,
    log: Arc<RecordingLog>,
    notifier: Arc<RecordingNotifier>,
    config: Arc<StaticConfig>,
    fake: Arc<FakeClient>,
    engine: Arc<Engine>,
}

impl Harness {
    fn new(
        label: &str,
        at: (i32, u32, u32, u32, u32),
        config: GenConfig,
        responses: Vec<Result<String, ChatError>>,
    ) -> Self {
        let fake = FakeClient::new(responses);
        let client: Arc<dyn ChatClient> = fake.clone();
        Self::build(label, at, config, fake, client)
    }

    fn with_http(label: &str, at: (i32, u32, u32, u32, u32), config: GenConfig) -> Self {
        let client: Arc<dyn ChatClient> =
            Arc::new(ReqwestChatClient::with_settings(Duration::from_secs(10), true));
        Self::with_client(label, at, config, client)
    }

    fn with_client(
        label: &str,
        at: (i32, u32, u32, u32, u32),
        config: GenConfig,
        client: Arc<dyn ChatClient>,
    ) -> Self {
        let fake = FakeClient::new(Vec::new());
        Self::build(label, at, config, fake, client)
    }

    fn build(
        label: &str,
        at: (i32, u32, u32, u32, u32),
        config: GenConfig,
        fake: Arc<FakeClient>,
        client: Arc<dyn ChatClient>,
    ) -> Self {
        let paths = TestPaths::new(label);
        let clock = ManualClock::at(at.0, at.1, at.2, at.3, at.4);
        let log = Arc::new(RecordingLog::default());
        let notifier = Arc::new(RecordingNotifier::default());
        let config = StaticConfig::new(config);

        let engine = Arc::new(Engine {
            paths: paths.gen_paths(),
            config: config.clone(),
            client,
            clock: clock.clone(),
            log: log.clone(),
            notifier: notifier.clone(),
            retry: RetryPolicy {
                attempts: 3,
                base_delay: Duration::ZERO,
                max_delay: Duration::ZERO,
            },
            state: Mutex::new(EngineState::default()),
        });

        Self {
            paths,
            clock,
            log,
            notifier,
            config,
            fake,
            engine,
        }
    }

    fn generate(&self, day: &str, trigger: Trigger) -> Outcome {
        self.engine.generate_day(day, trigger)
    }

    fn call_count(&self) -> usize {
        self.fake.call_count()
    }

    fn requests(&self) -> Vec<ChatRequest> {
        self.fake.requests()
    }

    fn log_text(&self) -> String {
        self.log.joined()
    }
}

fn http_error(status: u16) -> ChatError {
    ChatError::Http {
        status,
        body: format!("{{\"error\":{{\"message\":\"mock {status}\"}}}}"),
    }
}

// ---------------------------------------------------------------------------
// R12-1 / 验收 1：成功路径（v3.0：不要求正文出现时间戳）
// ---------------------------------------------------------------------------

#[test]
fn success_path_writes_output_without_timestamp_requirement() {
    let harness = Harness::new(
        "success",
        (2026, 11, 16, 8, 0),
        config_with_key(),
        vec![Ok(
            "他早上醒了一次，看了眼时间又倒回去。中午的泡面泡得有点久，凉了也没在意。"
                .to_string(),
        )],
    );
    harness.paths.write_data(
        "2026-11-15",
        "08:47 · 醒。看了眼时间，又睡了 · 想躲\n11:40 · 泡面，配一个讲时间管理的视频\n",
    );

    let outcome = harness.generate("2026-11-15", Trigger::Backfill);

    match outcome {
        Outcome::Written { chars, calls, .. } => {
            assert!(chars <= 800, "字数应 ≤800，实际 {chars}");
            assert_eq!(calls, 1, "成功路径只应调用一次 API");
        }
        other => panic!("应写入 output，实际 {other:?}"),
    }

    let text = harness
        .paths
        .output_text("2026-11-15")
        .expect("应生成 output");
    assert!(text.chars().count() <= 800);
    assert!(!text.ends_with('\n'), "不写尾随换行");
    assert!(
        output::collect_timestamps(&text).is_empty(),
        "v3.0 输出可以不出现时间戳"
    );
    assert_eq!(harness.call_count(), 1);
    assert!(
        harness.notifier.messages().is_empty(),
        "补跑成功默认不弹屏底提示"
    );
    assert!(
        harness.log_text().contains("[补跑] 2026-11-15 完成"),
        "日志应记录完成: {}",
        harness.log_text()
    );
    assert!(
        harness.log_text().contains("正文含时间戳 0 处"),
        "日志应记录正文时间戳处数: {}",
        harness.log_text()
    );
}

// ---------------------------------------------------------------------------
// R12-2：0 条记录
// ---------------------------------------------------------------------------

#[test]
fn zero_records_skips_without_api_call() {
    let harness = Harness::new(
        "zero-records",
        (2026, 11, 16, 8, 0),
        config_with_key(),
        Vec::new(),
    );
    harness.paths.write_data("2026-11-15", "\n\n");

    let outcome = harness.generate("2026-11-15", Trigger::Scheduled);

    assert_eq!(
        outcome,
        Outcome::Skipped {
            day: "2026-11-15".to_string(),
            reason: SkipReason::NoRecords,
        }
    );
    assert_eq!(harness.call_count(), 0);
    assert!(!harness.paths.output_path("2026-11-15").exists());
    assert!(harness.notifier.messages().is_empty());
    assert!(harness.log_text().contains("记录 0 条"));
}

#[test]
fn missing_data_file_is_skipped() {
    let harness = Harness::new(
        "missing-data",
        (2026, 11, 16, 8, 0),
        config_with_key(),
        Vec::new(),
    );

    let outcome = harness.generate("2026-11-15", Trigger::Backfill);

    assert_eq!(
        outcome,
        Outcome::Skipped {
            day: "2026-11-15".to_string(),
            reason: SkipReason::DataMissing,
        }
    );
    assert_eq!(harness.call_count(), 0);
    assert!(!harness.paths.output_path("2026-11-15").exists());
}

#[test]
fn all_invalid_lines_are_treated_as_zero_records() {
    let harness = Harness::new(
        "invalid-lines",
        (2026, 11, 16, 8, 0),
        config_with_key(),
        Vec::new(),
    );
    harness
        .paths
        .write_data("2026-11-15", "这不是记录\n25:00 · 非法时间\n08:47 · 走神\n");

    let outcome = harness.generate("2026-11-15", Trigger::Backfill);

    assert_eq!(
        outcome,
        Outcome::Skipped {
            day: "2026-11-15".to_string(),
            reason: SkipReason::NoRecords,
        }
    );
    assert_eq!(harness.call_count(), 0);
}

// ---------------------------------------------------------------------------
// R12-3：无 key
// ---------------------------------------------------------------------------

#[test]
fn missing_key_skips_without_api_call() {
    let harness = Harness::new(
        "no-key",
        (2026, 11, 16, 8, 0),
        config_without_key(),
        Vec::new(),
    );
    harness.paths.write_data("2026-11-15", "08:47 · 醒\n");

    let outcome = harness.generate("2026-11-15", Trigger::Scheduled);

    assert_eq!(
        outcome,
        Outcome::Skipped {
            day: "2026-11-15".to_string(),
            reason: SkipReason::NoKey,
        }
    );
    assert_eq!(harness.call_count(), 0);
    assert!(!harness.paths.output_path("2026-11-15").exists());
    assert!(harness.notifier.messages().is_empty());
    assert!(harness.log_text().contains("未配置 API key"));
}

// ---------------------------------------------------------------------------
// R12-4：连续失败 3 次
// ---------------------------------------------------------------------------

#[test]
fn api_failure_gives_up_after_three_attempts() {
    let harness = Harness::new(
        "api-failure",
        (2026, 11, 16, 8, 0),
        config_with_key(),
        vec![Err(http_error(500)), Err(http_error(500)), Err(http_error(500))],
    );
    harness.paths.write_data("2026-11-15", "08:47 · 醒\n");

    let outcome = harness.generate("2026-11-15", Trigger::Scheduled);

    match outcome {
        Outcome::Failed { calls, reason, .. } => {
            assert_eq!(calls, 3);
            assert!(reason.contains("500"), "原因应含状态码: {reason}");
        }
        other => panic!("应失败，实际 {other:?}"),
    }
    assert_eq!(harness.call_count(), 3);
    assert_eq!(harness.clock.sleep_count(), 2, "3 次尝试之间应有 2 次退避等待");
    assert!(!harness.paths.output_path("2026-11-15").exists());
    assert!(harness.notifier.messages().is_empty());
    let failures = harness.log.count("次 HTTP 尝试失败");
    assert_eq!(failures, 3, "日志应能查到 3 次尝试失败");
    assert!(
        harness.log_text().contains("第 1 轮第 1/3 次 HTTP 尝试失败"),
        "日志应区分「轮次」与「第几次尝试」: {}",
        harness.log_text()
    );
}

// ---------------------------------------------------------------------------
// R12-5 / 验收 5：超 800 字符 → 重试一次 → 截断兜底
// ---------------------------------------------------------------------------

#[test]
fn oversized_output_is_retried_then_truncated_to_limit() {
    let long_text = "啊".repeat(900);
    let harness = Harness::new(
        "oversized",
        (2026, 11, 16, 8, 0),
        config_with_key(),
        vec![Ok(long_text.clone()), Ok(long_text)],
    );
    harness.paths.write_data("2026-11-15", "08:47 · 醒\n");

    let outcome = harness.generate("2026-11-15", Trigger::Backfill);

    match outcome {
        Outcome::Written { chars, calls, .. } => {
            assert_eq!(calls, 2, "超限应先重试一次更严格指令");
            assert_eq!(chars, 800, "第二轮仍超限应截断到 800 字符");
        }
        other => panic!("应写入（截断兜底），实际 {other:?}"),
    }
    let text = harness
        .paths
        .output_text("2026-11-15")
        .expect("应生成 output");
    assert_eq!(text.chars().count(), 800);
    assert!(harness.log_text().contains("截断"), "日志应说明兜底方式");
    assert!(
        harness.log_text().contains("第 1 轮输出未通过校验"),
        "日志应记录校验失败轮次: {}",
        harness.log_text()
    );
    assert!(
        harness.log_text().contains("第 2 轮仍超限"),
        "日志应区分第 2 轮与最终兜底: {}",
        harness.log_text()
    );
}

// ---------------------------------------------------------------------------
// R12-6：output 已存在
// ---------------------------------------------------------------------------

#[test]
fn existing_output_is_skipped_without_calling_api() {
    let harness = Harness::new(
        "exists",
        (2026, 11, 16, 8, 0),
        config_with_key(),
        Vec::new(),
    );
    harness.paths.write_data("2026-11-15", "08:47 · 醒\n");
    std::fs::write(harness.paths.output_path("2026-11-15"), "已有旁白").expect("预置 output");

    let outcome = harness.generate("2026-11-15", Trigger::Scheduled);

    assert_eq!(
        outcome,
        Outcome::Skipped {
            day: "2026-11-15".to_string(),
            reason: SkipReason::OutputExists,
        }
    );
    assert_eq!(harness.call_count(), 0);
    assert_eq!(
        harness.paths.output_text("2026-11-15").as_deref(),
        Some("已有旁白"),
        "不覆盖已有文件"
    );
}

// ---------------------------------------------------------------------------
// R12-7：补跑多天
// ---------------------------------------------------------------------------

#[test]
fn backfill_only_fills_missing_days_and_continues_after_failure() {
    let harness = Harness::new(
        "backfill",
        (2026, 11, 16, 8, 0),
        config_with_key(),
        vec![
            Err(http_error(500)),
            Err(http_error(500)),
            Err(http_error(500)),
            Ok("11:40 他吸溜了一口。".to_string()),
        ],
    );
    harness.paths.write_data("2026-11-13", "08:47 · 旧的\n");
    harness.paths.write_data("2026-11-14", "09:00 · 已有旁白\n");
    harness.paths.write_data("2026-11-15", "11:40 · 泡面\n");
    std::fs::write(harness.paths.output_path("2026-11-14"), "已有旁白").expect("预置 output");

    let results = harness.engine.run_startup_backfill();

    assert_eq!(results.len(), 2, "只补缺 output 的两天");
    assert_eq!(results[0].0, "2026-11-13");
    assert!(
        matches!(results[0].1, Outcome::Failed { .. }),
        "第一天失败"
    );
    assert_eq!(results[1].0, "2026-11-15");
    assert!(
        matches!(results[1].1, Outcome::Written { .. }),
        "单天失败不应影响后一天"
    );
    assert_eq!(harness.call_count(), 4);
    assert_eq!(
        harness.paths.output_text("2026-11-14").as_deref(),
        Some("已有旁白")
    );
    assert!(harness.paths.output_path("2026-11-15").exists());
    assert!(
        harness.notifier.messages().is_empty(),
        "补跑成功默认不弹屏底提示"
    );
    assert!(harness.log_text().contains("[补跑] 结束"));
}

#[test]
fn backfill_skips_whole_batch_without_key() {
    let harness = Harness::new(
        "backfill-no-key",
        (2026, 11, 16, 8, 0),
        config_without_key(),
        Vec::new(),
    );
    harness.paths.write_data("2026-11-13", "08:47 · 旧的\n");
    harness.paths.write_data("2026-11-15", "11:40 · 泡面\n");

    let results = harness.engine.run_startup_backfill();

    assert!(results.is_empty());
    assert_eq!(harness.call_count(), 0);
    assert!(harness.log_text().contains("未配置 API key"));
}

#[test]
fn backfill_boundary_is_next_day_seven_am() {
    assert!(!schedule::is_backfill_due(
        local_time(2026, 11, 16, 6, 59),
        "2026-11-15"
    ));
    assert!(schedule::is_backfill_due(
        local_time(2026, 11, 16, 7, 0),
        "2026-11-15"
    ));
    assert!(schedule::is_backfill_due(
        local_time(2026, 11, 17, 0, 0),
        "2026-11-15"
    ));
    assert!(!schedule::is_backfill_due(
        local_time(2026, 11, 16, 6, 0),
        "2026-11-15"
    ));
    assert!(!schedule::is_backfill_due(
        local_time(2026, 11, 16, 8, 0),
        "2026-11-16"
    ));

    assert_eq!(
        schedule::backfill_cutoff("2026-11-15"),
        Some(local_time(2026, 11, 16, 7, 0))
    );
    assert_eq!(schedule::backfill_cutoff("2026-1-1"), None);
    assert_eq!(schedule::backfill_cutoff("not-a-date"), None);
}

#[test]
fn clock_boundaries_across_midnight_and_six_am() {
    // 凌晨 0–6 点：day_key 归前一天；此日 07:00 未到，不触发；补跑仍以「次日 07:00」为准。
    let after_midnight = local_time(2026, 11, 16, 1, 30);
    assert_eq!(crate::day::day_key(after_midnight), "2026-11-15");
    assert_eq!(crate::day::previous_day_key(after_midnight), "2026-11-14");
    assert!(schedule::is_backfill_due(after_midnight, "2026-11-14"));
    assert!(!schedule::is_backfill_due(after_midnight, "2026-11-15"));

    // 06:00 是跨天边界，但 07:00 才是生成时刻：06:00 时 11-15 仍不应补跑。
    let six_am = local_time(2026, 11, 16, 6, 0);
    assert_eq!(crate::day::day_key(six_am), "2026-11-16");
    assert!(!schedule::is_backfill_due(six_am, "2026-11-15"));
    assert!(!schedule::is_backfill_due(six_am, "2026-11-16"));
}

#[test]
fn scheduled_tick_does_not_fire_in_early_morning() {
    let harness = Harness::new(
        "tick-early",
        (2026, 11, 16, 1, 30),
        config_with_key(),
        Vec::new(),
    );
    harness.paths.write_data("2026-11-14", "08:47 · 醒\n");

    assert!(harness.engine.tick().is_none(), "凌晨 01:30 不应触发 07:00 生成");
    assert_eq!(harness.call_count(), 0, "不应调用 API");
    assert!(!harness.paths.output_path("2026-11-14").exists());
}

#[test]
fn backfill_days_lists_only_due_days_without_output() {
    let paths = TestPaths::new("backfill-days");
    paths.write_data("2026-11-13", "08:47 · 旧的\n");
    paths.write_data("2026-11-14", "08:47 · 已有旁白\n");
    paths.write_data("2026-11-15", "11:40 · 泡面\n");
    paths.write_data("2026-11-16", "12:00 · 今天\n");
    paths.write_data("not-a-day", "08:47 · 非法文件名\n");
    std::fs::write(paths.output_path("2026-11-14"), "已有旁白").expect("预置 output");

    let days = schedule::backfill_days(&paths.data_dir(), &paths.output_dir(), local_time(2026, 11, 16, 8, 0));

    assert_eq!(
        days,
        vec!["2026-11-13".to_string(), "2026-11-15".to_string()],
        "只补「次日 07:00 已过且缺 output」的天，按日期升序"
    );
}

// ---------------------------------------------------------------------------
// R16 / R17 / R18 / 验收 8、17、22：取消时间戳校验（只记计数，不做数字校验）
// ---------------------------------------------------------------------------

#[test]
fn output_without_any_timestamp_passes_without_retry() {
    let text = "他坐了回去，顺手把杯子推远了一点。";
    let harness = Harness::new(
        "no-timestamp",
        (2026, 11, 16, 8, 0),
        config_with_key(),
        vec![Ok(text.to_string())],
    );
    harness.paths.write_data("2026-11-15", "08:47 · 醒\n");

    let outcome = harness.generate("2026-11-15", Trigger::Backfill);

    assert!(
        matches!(outcome, Outcome::Written { calls: 1, .. }),
        "无时间戳输出应正常通过、不重试，实际 {outcome:?}"
    );
    assert_eq!(harness.call_count(), 1);
    assert_eq!(
        harness.paths.output_text("2026-11-15").as_deref(),
        Some(text)
    );
    assert!(
        harness.log_text().contains("正文含时间戳 0 处"),
        "日志应记录处数: {}",
        harness.log_text()
    );
}

#[test]
fn out_of_record_timestamps_no_longer_trigger_retry() {
    let text = "08:47 他坐了回去，03:33 又醒了。";
    let harness = Harness::new(
        "bad-timestamp",
        (2026, 11, 16, 8, 0),
        config_with_key(),
        vec![Ok(text.to_string())],
    );
    harness.paths.write_data("2026-11-15", "08:47 · 醒\n11:40 · 泡面\n");

    let outcome = harness.generate("2026-11-15", Trigger::Backfill);

    assert!(
        matches!(outcome, Outcome::Written { calls: 1, .. }),
        "记录外时间戳不再触发重试，实际 {outcome:?}"
    );
    assert_eq!(harness.call_count(), 1);
    assert_eq!(
        harness.paths.output_text("2026-11-15").as_deref(),
        Some(text)
    );
    assert!(
        harness.log_text().contains("正文含时间戳 2 处"),
        "日志应记录处数: {}",
        harness.log_text()
    );
}

/// R17/R18 / 验收 8、17、22：允许叙事引用时间，但程序侧仍不校验时间戳（只记计数）。
///
/// 「含 `21:30`」与「只有口语时间（无 `HH:MM`）」两种输出都一次通过、不重试；
/// 「钟点必须来自日志」由系统提示词五-10 约束，不做语义校验。
#[test]
fn r17_time_references_do_not_trigger_retry() {
    let timed = "他玩到 21:30，才忽然想起离散作业还没动。";
    let harness = Harness::new(
        "r17-timed",
        (2026, 11, 16, 8, 0),
        config_with_key(),
        vec![Ok(timed.to_string())],
    );
    harness
        .paths
        .write_data("2026-11-15", "09:12 · 起床，泡了杯咖啡\n20:05 · 打游戏\n");

    let outcome = harness.generate("2026-11-15", Trigger::Backfill);

    assert!(
        matches!(outcome, Outcome::Written { calls: 1, .. }),
        "含 21:30 的输出不应触发重试，实际 {outcome:?}"
    );
    assert_eq!(harness.call_count(), 1);
    assert_eq!(
        harness.paths.output_text("2026-11-15").as_deref(),
        Some(timed)
    );
    assert!(
        harness.log_text().contains("正文含时间戳 1 处"),
        "日志只统计处数: {}",
        harness.log_text()
    );
    assert_eq!(
        harness.requests()[0].system,
        prompt::SYSTEM_PROMPT,
        "一次通过时不得追加 ③ 重试指令"
    );

    // 口语时间（无 HH:MM）同样一次通过，日志处数为 0。
    let colloquial = "他玩到晚上九点半，才忽然想起离散作业还没动。";
    let harness = Harness::new(
        "r17-colloquial",
        (2026, 11, 16, 8, 0),
        config_with_key(),
        vec![Ok(colloquial.to_string())],
    );
    harness.paths.write_data(
        "2026-11-15",
        "20:05 · 打游戏\n21:30 · 想起离散作业还没动\n",
    );

    let outcome = harness.generate("2026-11-15", Trigger::Backfill);

    assert!(
        matches!(outcome, Outcome::Written { calls: 1, .. }),
        "口语时间不应触发重试，实际 {outcome:?}"
    );
    assert_eq!(harness.call_count(), 1);
    assert_eq!(
        harness.paths.output_text("2026-11-15").as_deref(),
        Some(colloquial)
    );
    assert!(
        harness.log_text().contains("正文含时间戳 0 处"),
        "日志只统计处数: {}",
        harness.log_text()
    );
}

// ---------------------------------------------------------------------------
// R12-9：07:00 触发（注入时钟）
// ---------------------------------------------------------------------------

#[test]
fn scheduled_tick_fires_once_per_calendar_day() {
    let harness = Harness::new(
        "tick-once",
        (2026, 11, 16, 6, 59),
        config_with_key(),
        vec![Ok("08:47 他坐了回去。".to_string())],
    );
    harness
        .paths
        .write_data("2026-11-15", "08:47 · 醒。看了眼时间，又睡了\n");

    assert!(harness.engine.tick().is_none(), "06:59 不应触发");
    assert_eq!(harness.call_count(), 0);

    harness.clock.set(2026, 11, 16, 7, 0);
    let (day, outcome) = harness.engine.tick().expect("07:00 应触发");
    assert_eq!(day, "2026-11-15", "目标日期应为「昨天」");
    assert_eq!(day, crate::day::previous_day_key(harness.clock.now()));
    assert!(matches!(outcome, Outcome::Written { .. }), "{outcome:?}");
    assert_eq!(harness.call_count(), 1);
    assert_eq!(harness.notifier.messages(), vec![TOAST_MESSAGE.to_string()]);

    assert!(harness.engine.tick().is_none(), "同一日历日第二次 tick 应跳过");
    harness.clock.set(2026, 11, 16, 7, 30);
    assert!(harness.engine.tick().is_none());
    harness.clock.set(2026, 11, 16, 23, 59);
    assert!(harness.engine.tick().is_none());
    assert_eq!(harness.call_count(), 1, "同一日历日只调用一次 API");
    assert_eq!(harness.notifier.messages().len(), 1, "只提示一次");
}

#[test]
fn scheduled_tick_resets_on_next_calendar_day() {
    let harness = Harness::new(
        "tick-reset",
        (2026, 11, 16, 7, 0),
        config_with_key(),
        vec![Ok("08:47 他坐了回去。".to_string())],
    );
    harness.paths.write_data("2026-11-15", "08:47 · 醒\n");

    let (first_day, first_outcome) = harness.engine.tick().expect("11-16 07:00 应触发");
    assert_eq!(first_day, "2026-11-15");
    assert!(matches!(first_outcome, Outcome::Written { .. }));

    harness.clock.set(2026, 11, 17, 6, 59);
    assert!(harness.engine.tick().is_none(), "次日 06:59 不应触发");

    harness.clock.set(2026, 11, 17, 7, 0);
    let (next_day, next_outcome) = harness.engine.tick().expect("次日 07:00 应再次触发");
    assert_eq!(next_day, "2026-11-16");
    assert_eq!(
        next_outcome,
        Outcome::Skipped {
            day: "2026-11-16".to_string(),
            reason: SkipReason::DataMissing,
        }
    );
    assert_eq!(harness.call_count(), 1);
}

// ---------------------------------------------------------------------------
// R11：去重标记（同日只处理一次）
// ---------------------------------------------------------------------------

#[test]
fn second_attempt_for_same_day_is_skipped_by_marker() {
    let harness = Harness::new(
        "attempted",
        (2026, 11, 16, 8, 0),
        config_with_key(),
        vec![Err(http_error(500)), Err(http_error(500)), Err(http_error(500))],
    );
    harness.paths.write_data("2026-11-15", "08:47 · 醒\n");

    assert!(matches!(
        harness.generate("2026-11-15", Trigger::Backfill),
        Outcome::Failed { .. }
    ));
    let second = harness.generate("2026-11-15", Trigger::Scheduled);

    assert_eq!(
        second,
        Outcome::Skipped {
            day: "2026-11-15".to_string(),
            reason: SkipReason::AlreadyAttempted,
        }
    );
    assert_eq!(harness.call_count(), 3, "不应重复调用 API");
    assert!(harness.log_text().contains("已尝试过该日期"));
}

// ---------------------------------------------------------------------------
// R14 / R7 边界
// ---------------------------------------------------------------------------

#[test]
fn empty_model_output_is_failure() {
    let harness = Harness::new(
        "empty-output",
        (2026, 11, 16, 8, 0),
        config_with_key(),
        vec![Ok("   \n".to_string()), Ok("   \n".to_string())],
    );
    harness.paths.write_data("2026-11-15", "08:47 · 醒\n");

    let outcome = harness.generate("2026-11-15", Trigger::Backfill);

    assert!(matches!(outcome, Outcome::Failed { calls: 2, .. }), "{outcome:?}");
    assert!(!harness.paths.output_path("2026-11-15").exists(), "不写空文件");
    assert!(harness.log_text().contains("正文为空"));
}

#[test]
fn code_fences_are_stripped_before_writing() {
    let harness = Harness::new(
        "fences",
        (2026, 11, 16, 8, 0),
        config_with_key(),
        vec![Ok("```markdown\n08:47 他坐了回去。\n```".to_string())],
    );
    harness.paths.write_data("2026-11-15", "08:47 · 醒\n");

    let outcome = harness.generate("2026-11-15", Trigger::Backfill);

    assert!(matches!(outcome, Outcome::Written { .. }), "{outcome:?}");
    assert_eq!(
        harness.paths.output_text("2026-11-15").as_deref(),
        Some("08:47 他坐了回去。")
    );
}

#[test]
fn char_limit_boundary_799_and_800_pass() {
    let body_799 = "啊".repeat(799);
    match output::validate(&body_799) {
        output::Validation::Ok { chars, .. } => assert_eq!(chars, 799),
        other => panic!("799 字符应通过，实际 {other:?}"),
    }

    let body_800 = "啊".repeat(800);
    match output::validate(&body_800) {
        output::Validation::Ok { chars, .. } => assert_eq!(chars, 800),
        other => panic!("800 字符应通过，实际 {other:?}"),
    }

    let body_801 = "啊".repeat(801);
    match output::validate(&body_801) {
        output::Validation::TooLong { chars, .. } => assert_eq!(chars, 801),
        other => panic!("801 字符应超限，实际 {other:?}"),
    }
}

#[test]
fn output_at_801_chars_triggers_strict_retry_then_truncates_to_800() {
    let text_801 = "啊".repeat(801);
    let harness = Harness::new(
        "limit-801",
        (2026, 11, 16, 8, 0),
        config_with_key(),
        vec![Ok(text_801.clone()), Ok(text_801)],
    );
    harness.paths.write_data("2026-11-15", "08:47 · 醒\n");

    let outcome = harness.generate("2026-11-15", Trigger::Backfill);

    match outcome {
        Outcome::Written { chars, calls, .. } => {
            assert_eq!(calls, 2, "801 字符应触发一轮更严格重试");
            assert_eq!(chars, 800, "仍超限应截断到 800 字符");
        }
        other => panic!("应写入（截断兜底），实际 {other:?}"),
    }
    let text = harness
        .paths
        .output_text("2026-11-15")
        .expect("应生成 output");
    assert_eq!(text.chars().count(), 800);

    // 第 2 轮：系统提示词只在末尾追加 ③，失败原因填进 {失败原因}；用户消息仍是完整原始日志。
    let requests = harness.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[1].system,
        prompt::stricter_system(&requests[0].system, "正文 801 字符，超过 800 字符上限")
    );
    assert!(requests[1].system.contains("【重写要求】"));
    assert!(
        requests[1].system.contains("超过 800 字符上限"),
        "失败原因应由程序填进 ③: {}",
        requests[1].system
    );
    assert_eq!(requests[1].user, requests[0].user, "重试必须重发完整原始日志");
    assert!(requests[1].user.contains("08:47 · 醒"));
    assert!(harness.log_text().contains("截断"), "日志应说明兜底方式");
}

#[test]
fn read_day_skips_invalid_lines_and_keeps_raw_lines() {
    let paths = TestPaths::new("read-day");
    paths.write_data(
        "2026-11-15",
        "08:47 · 醒 · 想躲\n\n25:00 · 非法时间\n11:40 · 泡面\n",
    );

    match output::read_day(&paths.data_path("2026-11-15")) {
        output::ReadOutcome::Read(records) => {
            assert_eq!(records.entries.len(), 2);
            assert_eq!(
                records.lines,
                vec![
                    "08:47 · 醒 · 想躲".to_string(),
                    "11:40 · 泡面".to_string()
                ]
            );
            assert_eq!(records.invalid_lines, 2, "空行与非法行都计入无效");
            assert_eq!(records.total_lines, 4);
            assert_eq!(records.entries[0].time, "08:47");
            assert_eq!(records.entries[0].mood.map(|mood| mood.as_str()), Some("想躲"));
        }
        other => panic!("应读到记录，实际 {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// R15：标题行对齐（整页文档编辑器允许 Markdown 标题）
// ---------------------------------------------------------------------------

#[test]
fn headings_are_skipped_and_records_still_generate() {
    let harness = Harness::new(
        "headings-with-records",
        (2026, 11, 16, 8, 0),
        config_with_key(),
        vec![Ok("08:47 他抬了下眼皮。\n11:40 泡面凉了。".to_string())],
    );
    harness.paths.write_data(
        "2026-11-15",
        "## 上午\n08:47 · 醒 · 想躲\n### 中午\n11:40 · 泡面\n",
    );

    let outcome = harness.generate("2026-11-15", Trigger::Backfill);

    match outcome {
        Outcome::Written { chars, calls, .. } => {
            assert!(chars <= 800);
            assert_eq!(calls, 1);
        }
        other => panic!("标题 + 2 条记录应正常生成，实际 {other:?}"),
    }

    let requests = harness.requests();
    assert_eq!(requests.len(), 1);
    assert!(
        !requests[0].user.contains("上午") && !requests[0].user.contains("中午"),
        "标题不得进入 prompt: {}",
        requests[0].user
    );
    assert!(requests[0].user.contains("08:47 · 醒 · 想躲"));
    assert!(requests[0].user.contains("11:40 · 泡面"));
    assert!(
        requests[0].user.contains("共 2 条"),
        "条数应按 2 计: {}",
        requests[0].user
    );
    assert!(
        harness.log.level_contains(LogLevel::Info, "跳过标题行 2 条"),
        "应有 INFO 记录标题行条数（不是 WARN）: {}",
        harness.log_text()
    );
    assert!(
        harness.log_text().contains("记录 2 条"),
        "记录条数不应把标题算进去: {}",
        harness.log_text()
    );
    assert_eq!(harness.log.count_level(LogLevel::Warn), 0, "标题行不得产生 WARN");
}

#[test]
fn only_headings_is_treated_as_zero_records() {
    let harness = Harness::new(
        "headings-only",
        (2026, 11, 16, 8, 0),
        config_with_key(),
        Vec::new(),
    );
    harness.paths.write_data("2026-11-15", "# 今天\n## 上午\n");

    let outcome = harness.generate("2026-11-15", Trigger::Scheduled);

    assert_eq!(
        outcome,
        Outcome::Skipped {
            day: "2026-11-15".to_string(),
            reason: SkipReason::NoRecords,
        }
    );
    assert_eq!(harness.call_count(), 0, "只有标题不应调用 API");
    assert!(!harness.paths.output_path("2026-11-15").exists());
    assert!(harness.notifier.messages().is_empty());
    assert!(
        harness.log_text().contains("记录 0 条"),
        "日志应记录 0 条原因: {}",
        harness.log_text()
    );
    assert!(
        harness.log_text().contains("标题行 2 条"),
        "日志应能看出标题行被单独计数: {}",
        harness.log_text()
    );
    assert_eq!(harness.log.count_level(LogLevel::Warn), 0);
}

#[test]
fn heading_lines_do_not_emit_warn() {
    let harness = Harness::new(
        "headings-no-warn",
        (2026, 11, 16, 8, 0),
        config_with_key(),
        vec![Ok("08:47 他坐了回去。".to_string())],
    );
    harness.paths.write_data("2026-11-15", "  # 今天\n08:47 · 醒\n");

    let outcome = harness.generate("2026-11-15", Trigger::Backfill);

    assert!(matches!(outcome, Outcome::Written { .. }), "{outcome:?}");
    assert_eq!(
        harness.log.count_level(LogLevel::Warn),
        0,
        "标题行不得产生 WARN: {}",
        harness.log_text()
    );
    assert!(
        harness.log.level_contains(LogLevel::Info, "跳过标题行 1 条"),
        "INFO 计数应存在: {}",
        harness.log_text()
    );
}

#[test]
fn heading_line_detection_matches_r15_format() {
    // 允许：行首 0–3 空格 + #×1..6 + 一个空格 + 文字
    assert!(output::is_heading_line("# 今天"));
    assert!(output::is_heading_line("## 上午"));
    assert!(output::is_heading_line("###### 六级"));
    assert!(output::is_heading_line("  ## 两个空格"));
    assert!(output::is_heading_line("   ### 三个空格"));

    // 不允许：4 个前导空格、7 个 #、# 后没有空格、# 后只有空白、普通记录行
    assert!(!output::is_heading_line("    # 四个空格不算"));
    assert!(!output::is_heading_line("####### 七个井号不算"));
    assert!(!output::is_heading_line("##上午"));
    assert!(!output::is_heading_line("## "));
    assert!(!output::is_heading_line("08:47 · 醒"));
    assert!(!output::is_heading_line(""));
}

#[test]
fn config_is_reloaded_for_each_generation() {
    let harness = Harness::new(
        "config-reload",
        (2026, 11, 16, 8, 0),
        config_without_key(),
        vec![Ok("08:47 他坐了回去。".to_string())],
    );
    harness.paths.write_data("2026-11-15", "08:47 · 醒\n");

    assert_eq!(
        harness.generate("2026-11-15", Trigger::Backfill),
        Outcome::Skipped {
            day: "2026-11-15".to_string(),
            reason: SkipReason::NoKey,
        }
    );

    harness.config.set_key(Some(FAKE_KEY.to_string()));
    let written = harness.generate("2026-11-15", Trigger::Backfill);
    assert!(matches!(written, Outcome::Written { .. }), "{written:?}");
}

// ---------------------------------------------------------------------------
// 纯函数单测（校验 / 截断 / 时间戳扫描 / 去围栏）
// ---------------------------------------------------------------------------

#[test]
fn truncate_counts_unicode_chars() {
    let text = "啊".repeat(820);
    let truncated = output::truncate_to_limit(&text);
    assert_eq!(truncated.chars().count(), 800);
    assert_eq!(truncated, "啊".repeat(800));
}

#[test]
fn timestamp_scan_finds_hhmm_and_rejects_digit_neighbours() {
    assert_eq!(
        output::collect_timestamps("08:47 他坐了回去。11:40 泡面。"),
        vec!["08:47".to_string(), "11:40".to_string()]
    );
    assert!(output::collect_timestamps("108:47 不是时间戳").is_empty());
    assert!(output::collect_timestamps("1:30 不是两位时").is_empty());
    assert_eq!(
        output::collect_timestamps("24:00 越界也算"),
        vec!["24:00".to_string()]
    );
}

#[test]
fn clean_strips_code_fences_and_whitespace() {
    assert_eq!(
        output::clean("  ```markdown\n08:47 他坐了回去。\n```  "),
        "08:47 他坐了回去。"
    );
    assert_eq!(output::clean("08:47 他坐了回去。\n"), "08:47 他坐了回去。");
    assert_eq!(output::clean("   "), "");
}

#[test]
fn write_output_is_atomic_and_leaves_no_temp_file() {
    let paths = TestPaths::new("atomic-write");
    let path = paths.output_path("2026-11-15");

    output::write_output(&path, "他坐了回去。").expect("写入应成功");

    assert_eq!(
        std::fs::read_to_string(&path).expect("读取输出文件"),
        "他坐了回去。"
    );
    let leftovers: Vec<String> = std::fs::read_dir(paths.output_dir())
        .expect("读输出目录")
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .filter(|name| name.ends_with(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "不应残留临时文件: {leftovers:?}");
}

// ---------------------------------------------------------------------------
// prompt（R16）：v3.0 逐字落库 + ② 用户消息模板 + ③ 重试追加段
// ---------------------------------------------------------------------------

/// v3.0 提示词文档（编译期内嵌，仅测试模块使用；生产提示词是逐字常量）。
const PROMPT_DOC: &str = include_str!("../../../docs/旁白提示词-v3.0.md");

/// 取文档中某个标题后第一个代码块的内容（CRLF 归一，去掉围栏行）。
fn doc_block(heading: &str) -> String {
    let normalized = PROMPT_DOC.replace("\r\n", "\n");
    let mut found = false;
    let mut inside = false;
    let mut collected: Vec<&str> = Vec::new();

    for line in normalized.split('\n') {
        if !found {
            if line.trim_end() == heading {
                found = true;
            }
            continue;
        }
        if !inside {
            if line.trim_start().starts_with("```") {
                inside = true;
            }
            continue;
        }
        if line.trim() == "```" {
            break;
        }
        collected.push(line);
    }

    assert!(found, "文档中应能找到标题：{heading}");
    assert!(inside, "标题 {heading} 后应有代码块");
    collected.join("\n")
}

#[test]
fn system_prompt_matches_v3_doc_verbatim() {
    let expected = doc_block("## ① 系统提示词");

    assert_eq!(
        prompt::SYSTEM_PROMPT, expected,
        "系统提示词必须与 docs/旁白提示词-v3.0.md ① 代码块逐字符一致"
    );
    assert_eq!(prompt::system_prompt(), expected);
    assert!(expected.contains("正文不超过 800 个字符（包含标点与空格）"));
    assert!(expected.contains("不要求逐条出现时间戳；如引用时间点，必须来自日志，不得编造。"));
    assert!(expected.contains("## 七、输出限制"));

    // R17/R18：三-3 / 五-10 / 七（输出限制）的时间线索条款必须逐字在常量里。
    assert!(expected.contains(
        "3. 时间引用是例外而非常规：全篇最多引用 3 个时间点，通常 1–2 处，只在关键转折或时间跨度大的事件上使用。段首尽量写人物或动作，只有确需强调时间跨度时才以时间词开头，且全篇最多 1 段以时间开头；全篇不足 3 段时不得以时间开头。写完后数一遍，把超出限制的时间引用删掉或挪到段中，改用「后来」「随后」等叙述词过渡。段首用时间时必须写成自然叙述（如「晚上八点出头，他才想起…」），不得写成「20:10，他…」这种裸时间戳标注；拿不准换算就只写「晚上」而不写具体钟点，宁可少写，不要写错。"
    ));
    assert!(expected.contains(
        "10. 引用钟点必须来自当天日志且换算正确：00–05 写「凌晨」、06–11 写「早上/上午」、12 写「中午」、13–17 写「下午」、18–23 写「晚上」（「夜里」「深夜」与「晚上」等价，可通用；「傍晚」只能用于 17–18 点，19 点后不得再写「傍晚」）；必须带时段词，如「20:10」写「晚上八点十分」（不得写成「十点」）、「23:10」写「晚上十一点十分」、「22:41」写「晚上十点四十一」、「00:30」写「凌晨零点三十」。可以概括为「晚上」「深夜」等不写具体钟点的说法，但不得编造日志里没有的钟点，也不得用「一上午」「一下午」「一整天」这类日志没有的时长概括；写时间区间（如「十点到十一点」）时，日志里必须同时有起止两个时间点。"
    ));
    assert!(expected.contains(
        "- （硬性）时间引用全篇不超过 3 处；全篇最多 1 段以时间开头，不足 3 段时不得以时间开头；不得以裸时间戳（如「20:10，」）开头。"
    ));
    assert!(expected.contains(
        "- 输出前自检：数一遍以时间开头的段落（最多 1 段，不足 3 段时为 0）和时间引用总数（最多 3 处）；超出就先删改再输出。"
    ));
    assert!(
        !expected.contains("不要求在正文中出现时间戳"),
        "v3.0 的旧表述应已被 R17 替换"
    );
    assert!(
        !expected.contains("将时间戳仅作为理解事件先后顺序的依据"),
        "v3.0 的三-3 旧表述应已被 R17 替换"
    );
    assert!(
        !expected.contains("不要逐条标注，也不得把时间戳当作固定的段落标题"),
        "R17 的三-3 旧表述应已被 R18 收紧替换"
    );
}

/// R18 第 7 条 + R19 第 4 条：修订记录修「七-4」→「七-5」，保留 v3.2，并记录 v3.3。
#[test]
fn prompt_doc_revision_record_is_fixed_and_versions_r18_r19() {
    let normalized = PROMPT_DOC.replace("\r\n", "\n");
    assert!(
        normalized.contains(
            "- v3.1（2026-11-17）：增加叙事中的时间线索条款（三-3、五-10、七-5），其余不变。"
        ),
        "修订记录应把 七-4 修成 七-5"
    );
    assert!(
        normalized.contains("- v3.2（2026-11-17）：时间引用收紧"),
        "修订记录应保留 v3.2 收紧条目"
    );
    assert!(
        normalized.contains("- v3.3（2026-11-17）：段首时间再收紧"),
        "修订记录应包含 v3.3 收紧条目"
    );
    assert!(
        !normalized.contains("三-3、五-10、七-4"),
        "不得残留错误的 七-4"
    );
}

#[test]
fn user_message_matches_v3_template() {
    let day = "2026-11-15";
    let records = vec![
        "08:47 · 醒。看了眼时间，又睡了 · 想躲".to_string(),
        "11:40 · 泡面，配一个讲时间管理的视频".to_string(),
    ];
    let expected = doc_block("## ② 用户消息模板")
        .replace("{YYYY-MM-DD}", day)
        .replace("{N}", &records.len().to_string())
        .replace("{原始记录行，一行一条}", &records.join("\n"));

    assert_eq!(prompt::user_message(day, &records), expected);
    assert_eq!(prompt::PromptPair::build(day, &records).user, expected);
    assert!(
        expected.starts_with(
            "请根据以下日志，创作一篇符合系统提示词要求的日常短篇小说。\n\n日期：2026-11-15\n\n当天记录（共 2 条）：\n\n"
        ),
        "引导句 + 日期 + 条数之间应有空行：{expected}"
    );
    assert!(
        expected.ends_with("11:40 · 泡面，配一个讲时间管理的视频"),
        "原始记录按顺序逐行追加：{expected}"
    );
    assert!(
        expected.contains("08:47 · 醒。看了眼时间，又睡了 · 想躲"),
        "保留心情尾巴：{expected}"
    );
}

#[test]
fn engine_sends_user_message_matching_v3_template() {
    let harness = Harness::new(
        "prompt-user-format",
        (2026, 11, 16, 8, 0),
        config_with_key(),
        vec![Ok("他坐在窗边，看了一会儿外面的雨。".to_string())],
    );
    harness.paths.write_data(
        "2026-11-15",
        "## 上午\n\
         08:47 · 醒。看了眼时间，又睡了 · 想躲\n\
         11:40 · 泡面，配一个讲时间管理的视频\n\
         16:30 · 便利店，买了冰咖啡，把饭团放回去了 · 走神\n",
    );

    let outcome = harness.generate("2026-11-15", Trigger::Backfill);
    assert!(matches!(outcome, Outcome::Written { .. }), "{outcome:?}");

    let records = vec![
        "08:47 · 醒。看了眼时间，又睡了 · 想躲".to_string(),
        "11:40 · 泡面，配一个讲时间管理的视频".to_string(),
        "16:30 · 便利店，买了冰咖啡，把饭团放回去了 · 走神".to_string(),
    ];
    let expected = doc_block("## ② 用户消息模板")
        .replace("{YYYY-MM-DD}", "2026-11-15")
        .replace("{N}", &records.len().to_string())
        .replace("{原始记录行，一行一条}", &records.join("\n"));

    let requests = harness.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].user, expected,
        "用户消息必须符合 ② 模板（标题不进 prompt、保留心情尾巴与原顺序）"
    );
    assert_eq!(requests[0].system, doc_block("## ① 系统提示词"));
}

#[test]
fn retry_note_matches_v3_doc_verbatim_with_reason_filled() {
    let reason = "正文 801 字符，超过 800 字符上限";
    let expected = doc_block("## ③ 校验失败时的重试追加段").replace("{失败原因}", reason);

    assert_eq!(
        prompt::stricter_note(reason),
        expected,
        "③ 除 {{失败原因}} 外必须逐字一致"
    );
    assert!(expected.starts_with("【重写要求】"));
    assert!(expected.contains("请重新阅读当天的全部原始日志，并重新创作"));
    assert!(expected.contains("正文不超过800个字符（包含标点与空格）"));
    assert!(!expected.contains("{失败原因}"), "程序必须填掉占位符");
}

#[test]
fn stricter_prompt_only_appends_and_keeps_system_body_intact() {
    let system = prompt::system_prompt();
    let reason = "正文 900 字符，超过 800 字符上限";
    let stricter = prompt::stricter_system(&system, reason);

    assert!(stricter.starts_with(&system), "系统提示词主体不得被改写");
    assert_eq!(
        stricter,
        format!("{system}\n\n{}", prompt::stricter_note(reason)),
        "更严格指令只能是追加"
    );
    assert!(
        !prompt::SYSTEM_PROMPT.contains("【重写要求】"),
        "③ 不得混进系统提示词主体"
    );
}

#[test]
fn prompt_contains_no_key_paths_or_username() {
    let harness = Harness::new(
        "prompt-safety",
        (2026, 11, 16, 8, 0),
        config_with_key(),
        vec![Ok("他坐了回去。".to_string())],
    );
    harness
        .paths
        .write_data("2026-11-15", "08:47 · 醒。看了眼时间，又睡了 · 想躲\n");

    harness.generate("2026-11-15", Trigger::Backfill);

    let requests = harness.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    let joined = format!("{}\n{}", request.system, request.user);

    assert!(!joined.contains(FAKE_KEY), "prompt 不得包含 key");
    assert!(
        !joined.contains(&harness.paths.root.display().to_string()),
        "prompt 不得包含本机路径"
    );
    assert!(!joined.contains("Users"), "prompt 不得包含用户名/用户目录");
    assert!(!joined.contains("api_key"));
    assert!(request.system.starts_with("# 角色：日常生活小说旁白"));
    assert!(request.system.contains("正文不超过 800 个字符"));
    assert!(request.system.contains("不要求逐条出现时间戳；如引用时间点，必须来自日志，不得编造。"));
    assert!(request.system.contains("时间引用全篇不超过 3 处"));
    assert!(request.system.contains("全篇最多 1 段以时间开头"));
    assert!(request.system.contains("只输出小说正文。"));
    assert!(
        !request.system.contains("不超过 500"),
        "不得残留旧的 500 字上限"
    );
    assert!(request.user.contains("日期：2026-11-15"));
    assert!(request.user.contains("当天记录（共 1 条）："));
    assert!(request
        .user
        .contains("08:47 · 醒。看了眼时间，又睡了 · 想躲"));
}

// ---------------------------------------------------------------------------
// 真实 reqwest 客户端 + 本地 mock HTTP 服务（R12：mock HTTP 覆盖）
// ---------------------------------------------------------------------------

#[test]
fn http_client_posts_chat_completions_with_bearer_header() {
    let server = MockServer::start(vec![MockResponse::json(
        200,
        r#"{"choices":[{"message":{"content":"08:47 他坐了回去。"}}]}"#,
    )]);
    let client = ReqwestChatClient::with_settings(Duration::from_secs(10), true);
    let request = ChatRequest {
        base_url: server.base_url(),
        model: "deepseek-chat".to_string(),
        api_key: FAKE_KEY.to_string(),
        system: "系统提示".to_string(),
        user: "用户消息".to_string(),
    };

    let content = client.complete(&request).expect("mock HTTP 请求应成功");
    assert_eq!(content, "08:47 他坐了回去。");
    assert_eq!(server.request_count(), 1);

    let head = server.request_head(0);
    assert!(
        head.starts_with("POST /chat/completions HTTP/1.1"),
        "请求行: {head}"
    );
    let lower = head.to_ascii_lowercase();
    assert!(
        lower.contains(&format!("authorization: bearer {}", FAKE_KEY.to_ascii_lowercase())),
        "应带 Bearer 鉴权头: {head}"
    );

    let body: serde_json::Value =
        serde_json::from_str(&server.request_body(0)).expect("请求体应是 JSON");
    assert_eq!(body["model"], "deepseek-chat");
    assert_eq!(body["stream"], false);
    assert_eq!(body["messages"][0]["role"], "system");
    assert_eq!(body["messages"][0]["content"], "系统提示");
    assert_eq!(body["messages"][1]["role"], "user");
    assert_eq!(body["messages"][1]["content"], "用户消息");
}

#[test]
fn http_error_reports_status_without_leaking_key() {
    let server = MockServer::start(vec![MockResponse::json(
        401,
        r#"{"error":{"message":"Authentication Fails"}}"#,
    )]);
    let client = ReqwestChatClient::with_settings(Duration::from_secs(10), true);
    let request = ChatRequest {
        base_url: server.base_url(),
        model: "deepseek-chat".to_string(),
        api_key: FAKE_KEY.to_string(),
        system: "系统提示".to_string(),
        user: "用户消息".to_string(),
    };

    let error = client.complete(&request).expect_err("401 应失败");
    assert!(matches!(error, ChatError::Http { status: 401, .. }), "{error:?}");
    let summary = error.summary();
    assert!(summary.contains("HTTP 401"), "{summary}");
    assert!(!summary.contains(FAKE_KEY), "错误摘要不得包含 key");
}

#[test]
fn end_to_end_generation_over_mock_http() {
    let server = MockServer::start(vec![MockResponse::json(
        200,
        r#"{"choices":[{"message":{"content":"08:47 他抬了下眼皮。\n11:40 泡面凉了。"}}]}"#,
    )]);
    let mut config = config_with_key();
    config.base_url = server.base_url();
    let harness = Harness::with_http("http-e2e", (2026, 11, 16, 8, 0), config);
    harness
        .paths
        .write_data("2026-11-15", "08:47 · 醒\n11:40 · 泡面\n");

    let outcome = harness.generate("2026-11-15", Trigger::Scheduled);

    assert!(
        matches!(outcome, Outcome::Written { calls: 1, .. }),
        "{outcome:?}"
    );
    assert_eq!(
        harness.paths.output_text("2026-11-15").as_deref(),
        Some("08:47 他抬了下眼皮。\n11:40 泡面凉了。")
    );
    assert_eq!(server.request_count(), 1);
    assert_eq!(harness.notifier.messages(), vec![TOAST_MESSAGE.to_string()]);
}

// ---------------------------------------------------------------------------
// R13：真实 DeepSeek 联调（默认忽略；只读应用配置里的 key，用临时目录）
// ---------------------------------------------------------------------------

/// 真实联调：`cargo test real_deepseek_success_path -- --ignored --nocapture`
///
/// - key 只从本机 `config.json` 只读读取（`Config::load()/api_key()`），不打印、不落盘、不进日志；
/// - data/output 用临时目录，不碰用户真实数据；
/// - 取不到 key 时打印说明并跳过（不判失败），由汇报标注「真实调用未验证」；
/// - 默认用合成样例（贴近 2026-11-16 的傍晚–深夜分布）；设置 `P2_REAL_DATA_DAY=YYYY-MM-DD`
///   时，从本机真实 `data/{day}.md` **只读**复制到临时目录再生成；
/// - R18/R19 人工检查：打印正文、段落数、每个 `HH:MM` 的日志溯源，便于核对
///   「时间总数 ≤3、以时间开头段落 ≤1、口语换算正确、无裸时间戳段首」。
#[test]
#[ignore = "真实 DeepSeek 联调：需要本机已保存 key，手动执行 --ignored"]
fn real_deepseek_success_path() {
    let config = crate::config::Config::load();
    let Some(api_key) = config.api_key() else {
        eprintln!("[真实联调] 应用配置里没有 API key，跳过（不打印 key）");
        return;
    };

    let harness = Harness::with_client(
        "real-deepseek",
        (2026, 11, 16, 8, 0),
        GenConfig {
            api_key: Some(api_key),
            base_url: config.base_url.clone(),
            model: config.model.clone(),
        },
        Arc::new(ReqwestChatClient::new()),
    );

    let day = match std::env::var("P2_REAL_DATA_DAY") {
        Ok(value) if !value.trim().is_empty() => {
            let day = value.trim().to_string();
            let source = crate::paths::get().data_file(&day);
            let raw = std::fs::read_to_string(&source)
                .unwrap_or_else(|error| panic!("读取真实日志失败（只读，未写回）: {error}"));
            harness.paths.write_data(&day, &raw);
            eprintln!("[真实联调] 数据源：真实 data/{day}.md（只读复制到临时目录）");
            day
        }
        _ => {
            // 合成样例：贴近用户真实记录的形态（2026-11-16 集中在傍晚到深夜），
            // 给 R18「关键转折 / 时间跨度」留素材；不复制真实记录原文。
            harness.paths.write_data(
                "2026-11-15",
                "18:08 · 在学校把语法看完了，犯困，玩了一会儿游戏 · 想躲\n\
                 18:48 · 忽然想起离散作业还没动，交给大肥鱼先做一版\n\
                 20:11 · 开始跑 robin 的 rvc 模型，试了几版音频\n\
                 20:46 · 最后换成 8:2 的模型，清晰多了 · 做成了\n\
                 22:41 · 和朋友玩了一把，准备睡觉，明天早八\n",
            );
            "2026-11-15".to_string()
        }
    };

    let outcome = harness.generate(&day, Trigger::Scheduled);

    match &outcome {
        Outcome::Written {
            chars,
            calls,
            elapsed_ms,
            ..
        } => {
            let text = harness.paths.output_text(&day).expect("应生成 output");
            let stamps = output::collect_timestamps(&text);
            let record_times: Vec<String> =
                match output::read_day(&harness.paths.data_path(&day)) {
                    output::ReadOutcome::Read(records) => {
                        records.entries.iter().map(|entry| entry.time.clone()).collect()
                    }
                    _ => Vec::new(),
                };
            eprintln!(
                "[真实联调] 成功：{chars} 字符（v3.3，上限 800），HTTP 调用 {calls} 次，耗时 {elapsed_ms}ms，正文含时间戳 {} 处",
                stamps.len()
            );
            eprintln!("[真实联调] 日志时间点：{}", record_times.join("、"));
            eprintln!("[真实联调] 正文：\n{text}");
            let paragraphs: Vec<&str> = text
                .split("\n\n")
                .map(str::trim)
                .filter(|paragraph| !paragraph.is_empty())
                .collect();
            eprintln!("[真实联调] 段落数：{}", paragraphs.len());
            for (index, paragraph) in paragraphs.iter().enumerate() {
                let head: String = paragraph.chars().take(16).collect();
                eprintln!("  p{}: {head}", index + 1);
            }
            eprintln!("[真实联调] 时间引用溯源：");
            for stamp in &stamps {
                let origin = if record_times.contains(stamp) {
                    "来自当天日志"
                } else {
                    "日志中不存在（不符合 R18/R19 五-10）"
                };
                eprintln!("  - {stamp}（{origin}）");
            }
            assert!(*chars <= 800, "正文应 ≤800 字符，实际 {chars}");
            assert_eq!(text.chars().count(), *chars, "文件字数应与结果一致");
            assert_eq!(harness.notifier.messages(), vec![TOAST_MESSAGE.to_string()]);
        }
        other => {
            eprintln!("[真实联调] 未成功：{other:?}");
            let messages = harness.log.messages();
            let start = messages.len().saturating_sub(6);
            eprintln!("[真实联调] 日志尾部：");
            for line in &messages[start..] {
                eprintln!("  {line}");
            }
            panic!("真实 DeepSeek 调用未成功：{other:?}");
        }
    }
}

// ---------------------------------------------------------------------------
// R8：先写文件、后弹提示
// ---------------------------------------------------------------------------

struct FileAwareNotifier {
    output_path: PathBuf,
    observed: Mutex<Vec<(String, bool)>>,
}

impl Notifier for FileAwareNotifier {
    fn notify(&self, message: &str) {
        let existed = self.output_path.exists();
        self.observed
            .lock()
            .expect("提示观察锁")
            .push((message.to_string(), existed));
    }
}

#[test]
fn output_file_exists_before_toast_is_sent() {
    let paths = TestPaths::new("toast-order");
    paths.write_data("2026-11-15", "08:47 · 醒\n");

    let notifier = Arc::new(FileAwareNotifier {
        output_path: paths.output_path("2026-11-15"),
        observed: Mutex::new(Vec::new()),
    });
    let client = FakeClient::new(vec![Ok("08:47 他坐了回去。".to_string())]);
    let engine = Engine {
        paths: paths.gen_paths(),
        config: StaticConfig::new(config_with_key()),
        client: client.clone(),
        clock: ManualClock::at(2026, 11, 16, 8, 0),
        log: Arc::new(RecordingLog::default()),
        notifier: notifier.clone(),
        retry: RetryPolicy {
            attempts: 3,
            base_delay: Duration::ZERO,
            max_delay: Duration::ZERO,
        },
        state: Mutex::new(EngineState::default()),
    };

    let outcome = engine.generate_day("2026-11-15", Trigger::Scheduled);

    assert!(matches!(outcome, Outcome::Written { .. }), "{outcome:?}");
    let observed = notifier.observed.lock().expect("提示观察锁");
    assert_eq!(observed.len(), 1, "定时成功应提示一次");
    assert_eq!(observed[0].0, TOAST_MESSAGE);
    assert!(
        observed[0].1,
        "提示发出时 output 文件应已存在（先写文件后提示）"
    );
}

/// 本地 doc 上补一个对 `ai` 模块的引用，避免模块仅被类型引用时的未使用告警。
#[test]
fn ai_module_is_reachable() {
    let _ = ai::REQUEST_TIMEOUT;
}
