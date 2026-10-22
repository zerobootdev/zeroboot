use axum::extract::{ConnectInfo, Json, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use std::fs::File;
use std::net::SocketAddr;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::vmm::kvm::{ForkedVm, VmSnapshot};

// --- Request/Response types ---

#[derive(Deserialize)]
pub struct ExecRequest {
    pub code: String,
    #[serde(default = "default_language")]
    pub language: String,
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u64,
}

fn default_language() -> String { "python".to_string() }
fn default_timeout() -> u64 { 30 }

#[derive(Serialize)]
pub struct ExecResponse {
    pub id: String,
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    pub fork_time_ms: f64,
    pub exec_time_ms: f64,
    pub total_time_ms: f64,
}

#[derive(Deserialize)]
pub struct BatchRequest {
    pub executions: Vec<ExecRequest>,
}

#[derive(Serialize)]
pub struct BatchResponse {
    pub results: Vec<ExecResponse>,
}

#[derive(Serialize)]
pub struct HealthResponse {
    pub status: String,
    pub templates: HashMap<String, TemplateStatus>,
}

#[derive(Serialize)]
pub struct TemplateStatus {
    pub ready: bool,
    pub numpy: bool,
}

#[derive(Serialize)]
pub struct ErrorResponse {
    pub error: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
}

// --- Rate Limiter ---

pub struct TokenBucket {
    tokens: f64,
    last_refill: Instant,
    rate: f64,    // tokens per second
    capacity: f64,
}

impl TokenBucket {
    #[allow(dead_code)]
    fn new(rate: f64) -> Self {
        Self::with_capacity(rate, rate.max(1.0))
    }
    fn with_capacity(rate: f64, capacity: f64) -> Self {
        Self { tokens: capacity, last_refill: Instant::now(), rate, capacity }
    }
    fn try_consume(&mut self) -> bool {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.rate).min(self.capacity);
        self.last_refill = now;
        if self.tokens >= 1.0 { self.tokens -= 1.0; true } else { false }
    }
}

// --- App State ---

pub struct Template {
    pub snapshot: VmSnapshot,
    pub memfd: i32,
    pub block_file: Option<Arc<File>>,
}

pub struct AppState {
    pub templates: HashMap<String, Template>,
    pub api_keys: Vec<String>,
    pub rate_limiters: Mutex<HashMap<String, TokenBucket>>,
    pub metrics: Metrics,
}

// Prometheus histogram with fixed bucket boundaries (in milliseconds).
const HISTOGRAM_BUCKETS_MS: &[f64] = &[0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1000.0];
const NUM_BUCKETS: usize = 13; // must match HISTOGRAM_BUCKETS_MS.len()

pub struct Histogram {
    // One counter per finite bucket + one for +Inf (index NUM_BUCKETS)
    buckets: [AtomicU64; NUM_BUCKETS + 1],
    sum_us: AtomicU64, // sum in microseconds for precision
    count: AtomicU64,
}

impl Histogram {
    fn new() -> Self {
        Self {
            buckets: std::array::from_fn(|_| AtomicU64::new(0)),
            sum_us: AtomicU64::new(0),
            count: AtomicU64::new(0),
        }
    }

    /// Record a value in milliseconds.
    fn observe(&self, value_ms: f64) {
        // Place into the first bucket where value <= bound, or +Inf overflow
        let slot = HISTOGRAM_BUCKETS_MS.iter()
            .position(|&bound| value_ms <= bound)
            .unwrap_or(NUM_BUCKETS);
        self.buckets[slot].fetch_add(1, Ordering::Relaxed);
        self.sum_us.fetch_add((value_ms * 1000.0) as u64, Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Relaxed);
    }

    /// Render as Prometheus histogram text. `name` is the metric base name (e.g. "zeroboot_fork_time_milliseconds").
    fn render(&self, name: &str, help: &str) -> String {
        let mut out = format!(
            "# HELP {} {}\n# TYPE {} histogram\n",
            name, help, name
        );
        let mut cumulative = 0u64;
        for (i, &bound) in HISTOGRAM_BUCKETS_MS.iter().enumerate() {
            cumulative += self.buckets[i].load(Ordering::Relaxed);
            out.push_str(&format!(
                "{}_bucket{{le=\"{}\"}} {}\n",
                name, format_bucket(bound), cumulative
            ));
        }
        cumulative += self.buckets[NUM_BUCKETS].load(Ordering::Relaxed);
        out.push_str(&format!("{}_bucket{{le=\"+Inf\"}} {}\n", name, cumulative));
        let sum_us = self.sum_us.load(Ordering::Relaxed);
        let count = self.count.load(Ordering::Relaxed);
        out.push_str(&format!("{}_sum {}\n", name, sum_us as f64 / 1000.0));
        out.push_str(&format!("{}_count {}\n", name, count));
        out
    }
}

fn format_bucket(v: f64) -> String {
    if v == v.floor() { format!("{}", v as u64) } else { format!("{}", v) }
}

pub struct Metrics {
    pub total_executions: AtomicU64,
    pub total_errors: AtomicU64,
    pub total_timeouts: AtomicU64,
    pub concurrent_forks: AtomicU64,
    pub fork_time_sum_us: AtomicU64,
    pub exec_time_sum_us: AtomicU64,
    pub fork_time_hist: Histogram,
    pub exec_time_hist: Histogram,
    pub total_time_hist: Histogram,
}

impl Metrics {
    pub fn new() -> Self {
        Self {
            total_executions: AtomicU64::new(0),
            total_errors: AtomicU64::new(0),
            total_timeouts: AtomicU64::new(0),
            concurrent_forks: AtomicU64::new(0),
            fork_time_sum_us: AtomicU64::new(0),
            exec_time_sum_us: AtomicU64::new(0),
            fork_time_hist: Histogram::new(),
            exec_time_hist: Histogram::new(),
            total_time_hist: Histogram::new(),
        }
    }
}


// --- Auth helper ---

fn extract_api_key(headers: &HeaderMap) -> Option<String> {
    headers.get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|s| s.to_string())
}

fn check_auth(state: &AppState, headers: &HeaderMap) -> Result<String, (StatusCode, Json<ErrorResponse>)> {
    if state.api_keys.is_empty() {
        return Ok("anonymous".to_string());
    }
    match extract_api_key(headers) {
        Some(key) if state.api_keys.contains(&key) => Ok(key),
        Some(_) => Err((StatusCode::UNAUTHORIZED, Json(ErrorResponse {
            error: "Invalid API key".to_string(), request_id: None,
        }))),
        None => Err((StatusCode::UNAUTHORIZED, Json(ErrorResponse {
            error: "Missing Authorization header".to_string(), request_id: None,
        }))),
    }
}

fn extract_client_ip(headers: &HeaderMap, addr: SocketAddr) -> String {
    // CF-Connecting-IP (set by Cloudflare)
    if let Some(v) = headers.get("cf-connecting-ip").and_then(|v| v.to_str().ok()) {
        let ip = v.trim();
        if !ip.is_empty() { return ip.to_string(); }
    }
    // X-Forwarded-For (first IP in the chain is the real client)
    if let Some(v) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
        if let Some(first) = v.split(',').next() {
            let ip = first.trim();
            if !ip.is_empty() { return ip.to_string(); }
        }
    }
    // Fall back to socket address
    addr.ip().to_string()
}

fn check_rate_limit(state: &AppState, key: &str, headers: &HeaderMap, addr: SocketAddr) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    if state.api_keys.is_empty() { return Ok(()); }
    let is_demo = key.starts_with("zb_demo_");
    let (bucket_key, rate, capacity) = if is_demo {
        let ip = extract_client_ip(headers, addr);
        (format!("demo:{}", ip), 10.0 / 60.0, 10.0)
    } else {
        (key.to_string(), 100.0, 100.0)
    };
    let mut limiters = state.rate_limiters.lock().unwrap();
    let bucket = limiters.entry(bucket_key).or_insert_with(|| TokenBucket::with_capacity(rate, capacity));
    if bucket.try_consume() { Ok(()) } else {
        let msg = if is_demo {
            "Rate limit exceeded (10 req/min for demo keys)"
        } else {
            "Rate limit exceeded (100 req/s)"
        };
        Err((StatusCode::TOO_MANY_REQUESTS, Json(ErrorResponse {
            error: msg.to_string(), request_id: None,
        })))
    }
}

// --- Request logging ---

const REQUEST_LOG_PATH: &str = "/var/log/zeroboot/requests.jsonl";

fn iso_now() -> String {
    let d = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap();
    let secs = d.as_secs();
    let millis = d.subsec_millis();
    // Compute UTC date/time from epoch seconds
    let days = secs / 86400;
    let time = secs % 86400;
    let h = time / 3600;
    let m = (time % 3600) / 60;
    let s = time % 60;
    // Days since 1970-01-01 to Y-M-D (civil_from_days algorithm)
    let z = days as i64 + 719468;
    let era = z / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z", y, mo, d, h, m, s, millis)
}

fn log_request(request_id: &str, client_ip: &str, api_key_masked: &str, language: &str, code: &str, response: &ExecResponse) {
    use std::io::Write;
    let line = serde_json::json!({
        "ts": iso_now(),
        "request_id": request_id,
        "client_ip": client_ip,
        "api_key": api_key_masked,
        "language": language,
        "code": code,
        "exit_code": response.exit_code,
        "fork_time_ms": response.fork_time_ms,
        "exec_time_ms": response.exec_time_ms,
        "total_time_ms": response.total_time_ms,
    });
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(REQUEST_LOG_PATH) {
        let _ = writeln!(f, "{}", line);
    }
}

// --- Exec helper ---

fn round2(v: f64) -> f64 { (v * 100.0).round() / 100.0 }

fn execute_code(state: &AppState, req: &ExecRequest, request_id: &str) -> ExecResponse {
    let total_start = Instant::now();
    state.metrics.concurrent_forks.fetch_add(1, Ordering::Relaxed);

    let result = (|| -> ExecResponse {
        let lang = if req.language == "node" || req.language == "javascript" { "node" } else { "python" };
        let template = match state.templates.get(lang) {
            Some(t) => t,
            None => {
                state.metrics.total_errors.fetch_add(1, Ordering::Relaxed);
                return ExecResponse {
                    id: request_id.to_string(), stdout: String::new(),
                    stderr: format!("No template for language: {}", req.language), exit_code: -1,
                    fork_time_ms: 0.0, exec_time_ms: 0.0,
                    total_time_ms: round2(total_start.elapsed().as_secs_f64() * 1000.0),
                };
            }
        };
        let mut vm = match ForkedVm::fork_cow(&template.snapshot, template.memfd, template.block_file.clone()) {
            Ok(vm) => vm,
            Err(e) => {
                state.metrics.total_errors.fetch_add(1, Ordering::Relaxed);
                return ExecResponse {
                    id: request_id.to_string(), stdout: String::new(),
                    stderr: format!("Fork failed: {}", e), exit_code: -1,
                    fork_time_ms: 0.0, exec_time_ms: 0.0,
                    total_time_ms: round2(total_start.elapsed().as_secs_f64() * 1000.0),
                };
            }
        };
        let fork_time_ms = round2(vm.fork_time_us / 1000.0);
        state.metrics.fork_time_sum_us.fetch_add(vm.fork_time_us as u64, Ordering::Relaxed);
        state.metrics.fork_time_hist.observe(vm.fork_time_us / 1000.0);

        let exec_start = Instant::now();
        let command = format!("CODE:{}
", req.code);
        if let Err(e) = vm.send_serial(command.as_bytes()) {
            state.metrics.total_errors.fetch_add(1, Ordering::Relaxed);
            return ExecResponse {
                id: request_id.to_string(), stdout: String::new(),
                stderr: format!("Send failed: {}", e), exit_code: -1,
                fork_time_ms, exec_time_ms: 0.0,
                total_time_ms: round2(total_start.elapsed().as_secs_f64() * 1000.0),
            };
        }

        let timeout_secs = req.timeout_seconds.min(300);
        let timeout = std::time::Duration::from_secs(timeout_secs);
        let result = vm.run_until_marker_timeout("ZEROBOOT_DONE", u64::MAX, Some(timeout));

        let exec_time_ms = round2(exec_start.elapsed().as_secs_f64() * 1000.0);
        state.metrics.exec_time_sum_us.fetch_add((exec_time_ms * 1000.0) as u64, Ordering::Relaxed);
        state.metrics.exec_time_hist.observe(exec_start.elapsed().as_secs_f64() * 1000.0);
        state.metrics.total_executions.fetch_add(1, Ordering::Relaxed);

        let (output, timed_out) = match result {
            Ok(s) => (s, false),
            Err(e) => {
                let msg = e.to_string();
                let is_timeout = msg.contains("timed out");
                if is_timeout {
                    state.metrics.total_timeouts.fetch_add(1, Ordering::Relaxed);
                } else {
                    state.metrics.total_errors.fetch_add(1, Ordering::Relaxed);
                }
                (msg, is_timeout)
            }
        };

        if timed_out {
            return ExecResponse {
                id: request_id.to_string(),
                stdout: String::new(),
                stderr: format!("Execution timed out after {}s", timeout_secs),
                exit_code: -1,
                fork_time_ms, exec_time_ms,
                total_time_ms: round2(total_start.elapsed().as_secs_f64() * 1000.0),
            };
        }

        let has_marker = output.contains("ZEROBOOT_DONE");
        let mut stdout = output.replace("ZEROBOOT_DONE", "").replace("\r\n", "\n").replace("\r", "");
        if let Some(pos) = stdout.find('\n') {
            let echo_prefix = format!("CODE:{}", req.code.trim());
            if stdout[..pos].trim() == echo_prefix.trim() {
                stdout = stdout[pos+1..].to_string();
            }
        }

        // Filter kernel log lines (e.g. "[18446744073.xxx] random: ...")
        stdout = stdout.lines()
            .filter(|l| !(l.trim_start().starts_with('[') && l.contains("] ")))
            .collect::<Vec<_>>()
            .join("
");

        let (exit_code, stderr) = if has_marker {
            (0, String::new())
        } else {
            state.metrics.total_errors.fetch_add(1, Ordering::Relaxed);
            (-1, "Process exited unexpectedly".to_string())
        };

        ExecResponse {
            id: request_id.to_string(),
            stdout: stdout.trim().to_string(),
            stderr,
            exit_code,
            fork_time_ms, exec_time_ms,
            total_time_ms: round2(total_start.elapsed().as_secs_f64() * 1000.0),
        }
    })();

    state.metrics.total_time_hist.observe(result.total_time_ms);
    state.metrics.concurrent_forks.fetch_sub(1, Ordering::Relaxed);
    result
}

// --- Handlers ---

pub async fn exec_handler(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(req): Json<ExecRequest>,
) -> impl IntoResponse {
    let api_key = match check_auth(&state, &headers) {
        Ok(k) => k,
        Err(e) => return e.into_response(),
    };
    if let Err(e) = check_rate_limit(&state, &api_key, &headers, addr) {
        return e.into_response();
    }

    let request_id = uuid::Uuid::now_v7().to_string();
    let rid = request_id.clone();
    let client_ip = extract_client_ip(&headers, addr);
    let language = req.language.clone();
    let code_snippet: String = req.code.chars().take(500).collect();
    let masked_key = if api_key.len() > 8 { format!("{}...{}", &api_key[..4], &api_key[api_key.len()-4..]) } else { "***".to_string() };

    let response = tokio::task::spawn_blocking(move || {
        execute_code(&state, &req, &rid)
    }).await.unwrap();

    log_request(&request_id, &client_ip, &masked_key, &language, &code_snippet, &response);

    (StatusCode::OK, Json(response)).into_response()
}

pub async fn batch_handler(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(req): Json<BatchRequest>,
) -> impl IntoResponse {
    let api_key = match check_auth(&state, &headers) {
        Ok(k) => k,
        Err(e) => return e.into_response(),
    };
    if let Err(e) = check_rate_limit(&state, &api_key, &headers, addr) {
        return e.into_response();
    }

    let client_ip = extract_client_ip(&headers, addr);
    let masked_key = if api_key.len() > 8 { format!("{}...{}", &api_key[..4], &api_key[api_key.len()-4..]) } else { "***".to_string() };

    let mut snippets = Vec::with_capacity(req.executions.len());
    let mut languages = Vec::with_capacity(req.executions.len());
    let mut request_ids = Vec::with_capacity(req.executions.len());
    let mut handles = Vec::with_capacity(req.executions.len());
    for exec_req in req.executions {
        let st = state.clone();
        let rid = uuid::Uuid::now_v7().to_string();
        snippets.push(exec_req.code.chars().take(500).collect::<String>());
        languages.push(exec_req.language.clone());
        request_ids.push(rid.clone());
        handles.push(tokio::task::spawn_blocking(move || {
            execute_code(&st, &exec_req, &rid)
        }));
    }

    let mut results = Vec::with_capacity(handles.len());
    for (i, h) in handles.into_iter().enumerate() {
        let response = h.await.unwrap();
        log_request(&request_ids[i], &client_ip, &masked_key, &languages[i], &snippets[i], &response);
        results.push(response);
    }

    (StatusCode::OK, Json(BatchResponse { results })).into_response()
}

pub async fn health_handler(
    State(state): State<Arc<AppState>>,
) -> Json<HealthResponse> {
    let mut templates = HashMap::new();
    for (name, _) in &state.templates {
        templates.insert(name.clone(), TemplateStatus {
            ready: true,
            numpy: name == "python",
        });
    }
    Json(HealthResponse { status: "ok".to_string(), templates })
}

pub async fn metrics_handler(
    State(state): State<Arc<AppState>>,
) -> String {
    let m = &state.metrics;
    let total = m.total_executions.load(Ordering::Relaxed);
    let errors = m.total_errors.load(Ordering::Relaxed);
    let timeouts = m.total_timeouts.load(Ordering::Relaxed);
    let concurrent = m.concurrent_forks.load(Ordering::Relaxed);
    let fork_sum = m.fork_time_sum_us.load(Ordering::Relaxed);
    let exec_sum = m.exec_time_sum_us.load(Ordering::Relaxed);

    let mut out = format!(
        "# HELP zeroboot_total_executions Total number of executions\n\
         # TYPE zeroboot_total_executions counter\n\
         zeroboot_total_executions{{status=\"success\"}} {}\n\
         zeroboot_total_executions{{status=\"error\"}} {}\n\
         zeroboot_total_executions{{status=\"timeout\"}} {}\n\
         # HELP zeroboot_concurrent_forks Current number of concurrent forks\n\
         # TYPE zeroboot_concurrent_forks gauge\n\
         zeroboot_concurrent_forks {}\n\
         # HELP zeroboot_fork_time_microseconds_total Sum of fork times\n\
         # TYPE zeroboot_fork_time_microseconds_total counter\n\
         zeroboot_fork_time_microseconds_total {}\n\
         # HELP zeroboot_exec_time_microseconds_total Sum of exec times\n\
         # TYPE zeroboot_exec_time_microseconds_total counter\n\
         zeroboot_exec_time_microseconds_total {}\n\
         # HELP zeroboot_memory_usage_bytes RSS memory usage\n\
         # TYPE zeroboot_memory_usage_bytes gauge\n\
         zeroboot_memory_usage_bytes {}\n",
        total.saturating_sub(errors).saturating_sub(timeouts), errors, timeouts,
        concurrent, fork_sum, exec_sum,
        get_rss_bytes(),
    );

    out.push_str(&m.fork_time_hist.render(
        "zeroboot_fork_time_milliseconds",
        "Histogram of VM fork times in milliseconds",
    ));
    out.push_str(&m.exec_time_hist.render(
        "zeroboot_exec_time_milliseconds",
        "Histogram of code execution times in milliseconds",
    ));
    out.push_str(&m.total_time_hist.render(
        "zeroboot_total_time_milliseconds",
        "Histogram of total request times in milliseconds",
    ));

    out
}

fn get_rss_bytes() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines().find(|l| l.starts_with("VmRSS:"))
                .and_then(|l| l.split_whitespace().nth(1).and_then(|v| v.parse::<u64>().ok()))
        })
        .unwrap_or(0) * 1024
}
