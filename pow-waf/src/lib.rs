pub mod chain;
pub mod config;

use chain::btc::BTC;
use config::Config;
use config::Setting;
use log::info;
use pow_runtime::counter_bucket::CounterBucket;
use pow_runtime::response::Response;
use pow_runtime::Ctx;
use pow_runtime::HttpHook;
use pow_runtime::{Runtime, RuntimeBox};
use pow_types::bytearray32::ByteArray32;
use pow_types::cidr::CIDR;
use pow_types::config::Router;
use proxy_wasm::traits::*;
use proxy_wasm::types::*;
use sha2::Digest;
use std::net::SocketAddr;
use std::sync::Arc;

proxy_wasm::main! {{
    proxy_wasm::set_log_level(LogLevel::Trace);
    proxy_wasm::set_root_context(move |context_id| -> Box<dyn RootContext> {
        Box::new(RuntimeBox::new(Plugin { context_id, inner: None }))
    });
}}

struct Inner {
    btc: BTC,
    router: Router<Setting>,
    counter_bucket: CounterBucket,
    whitelist: Vec<CIDR>,
    difficulty: u64,
}

#[derive(Clone)]
struct Plugin {
    context_id: u32,
    inner: Option<Arc<Inner>>,
}

impl Context for Plugin {}
impl Runtime for Plugin {
    type Hook = Hook;
    fn on_vm_start(&mut self, _vm_configuration_size: usize) -> bool {
        info!("PoW filter starting...");
        true
    }

    fn on_configure(&mut self, configuration: Option<Vec<u8>>) -> bool {
        info!("PoW filter configuring...");
        let Some(config_bytes) = configuration else {
            return false;
        };

        let mut config: Config<Setting> = match serde_yaml::from_slice(&config_bytes) {
            Ok(config) => config,
            Err(e) => {
                log::error!(
                    "failed to parse configuration: {}\n raw config: {}",
                    e,
                    String::from_utf8(config_bytes)
                        .expect("failed to read raw config into utf8 string")
                );
                return false;
            }
        };

        proxy_wasm::set_log_level(
            config
                .log_level
                .map(|l| l.into())
                .unwrap_or(LogLevel::Trace),
        );

        let whitelist = config.whitelist.take().unwrap_or_default();
        let difficulty = config.difficulty;
        let mempool_upstream_name = config.mempool_upstream_name.clone();

        let router: Router<Setting> = match config.virtual_hosts.try_into() {
            Ok(router) => router,
            Err(e) => {
                log::error!(
                    "failed to convert configuration: {}\n raw config: {}",
                    e,
                    String::from_utf8(config_bytes)
                        .expect("failed to read raw config into utf8 string")
                );
                return false;
            }
        };

        self.inner = Some(Arc::new(Inner {
            btc: BTC::new(mempool_upstream_name),
            router,
            counter_bucket: CounterBucket::new(self.context_id, "rate_limit"),
            whitelist,
            difficulty,
        }));
        info!("PoW filter configured");
        true
    }

    fn create_http_context(&self, _context_id: u32) -> Option<Self::Hook> {
        Some(Hook {
            ctx: Ctx::new(_context_id),
            plugin: self.inner.clone().expect("plugin not initialized"),
        })
    }
}

pub struct Hook {
    ctx: Ctx,
    plugin: Arc<Inner>,
}

fn transform_u64_to_u8_array(mut value: u64) -> [u8; 8] {
    let mut result = [0; 8];
    for i in 0..8 {
        result[7 - i] = (value & 0xff) as u8;
        value >>= 8;
    }
    result
}

/// Get the difficulty target as a big-endian 256-bit number.
/// The `level` parameter represents the number of leading zero bits required.
fn get_difficulty(level: u64) -> ByteArray32 {
    let mut difficulty = [0xff; 32];
    let initial = u64::MAX / level;
    let initial_bytes = transform_u64_to_u8_array(initial);
    difficulty[0..8].clone_from_slice(&initial_bytes);
    (&difficulty).into()
}

#[derive(serde::Serialize)]
struct DifficultyResponse {
    current: ByteArray32,
    difficulty: ByteArray32,
    error: String,
    message: String,
}

#[derive(Debug)]
enum Error {
    Status {
        reason: String,
        status: proxy_wasm::types::Status,
    },
    Response(Response),
    #[allow(dead_code)]
    Other {
        reason: String,
        error: Box<dyn std::error::Error>,
    },
}

impl Error {
    fn status(reason: impl Into<String>, status: proxy_wasm::types::Status) -> Self {
        Error::Status {
            reason: reason.into(),
            status,
        }
    }

    fn response(response: Response) -> Self {
        Error::Response(response)
    }

    #[allow(dead_code)]
    fn other(reason: impl Into<String>, error: impl Into<Box<dyn std::error::Error>>) -> Self {
        Error::Other {
            reason: reason.into(),
            error: error.into(),
        }
    }
}

impl From<Error> for Response {
    fn from(val: Error) -> Self {
        match val {
            Error::Response(response) => {
                log::debug!("reject request with response, {:?}", response.code);
                response
            }
            Error::Status { reason, status } => {
                let msg = format!("{:?}: {}", status, reason);
                log::warn!("failed hostcall with error, {}", msg);
                Response {
                    code: 500,
                    headers: vec![("Content-Type".to_string(), "text/plain".to_string())],
                    body: Some(msg.into_bytes()),
                    trailers: vec![],
                }
            }
            Error::Other { reason, error } => {
                let msg = format!("{}: {}", error, reason);
                log::warn!("failed unknow error, {}", msg);
                Response {
                    code: 500,
                    headers: vec![("Content-Type".to_string(), "text/plain".to_string())],
                    body: Some(msg.into_bytes()),
                    trailers: vec![],
                }
            }
        }
    }
}

fn too_many_request(current: ByteArray32, difficulty: u64, error: String) -> Error {
    let target = get_difficulty(difficulty);
    let body = DifficultyResponse {
        current,
        difficulty: target,
        error,
        message: "Access restriction triggered".to_string(),
    };
    Error::response(Response {
        code: 429,
        headers: vec![("Content-Type".to_string(), "application/json".to_string())],
        body: Some(
            serde_json::to_string(&body)
                .expect("failed to serialize difficulty")
                .into_bytes(),
        ),
        trailers: vec![],
    })
}

fn forbidden(message: String) -> Error {
    let body = serde_json::json!({ "message": message });
    Error::response(Response {
        code: 403,
        headers: vec![("Content-Type".to_string(), "text/json".to_string())],
        body: Some(body.to_string().into_bytes()),
        trailers: vec![],
    })
}

impl Hook {
    fn get_header(&self, key: &str) -> Result<String, Error> {
        self.ctx
            .get_http_request_header(key)
            .map_err(|s| Error::status(format!("failed to get header: {}", key), s))?
            .ok_or_else(|| forbidden(format!("missing header: {}", key)))
    }

    fn get_client_address(&self) -> Result<String, Error> {
        self.ctx
            .get_client_address()
            .map_err(|s| Error::status("failed to get client address", s))?
            .ok_or_else(|| forbidden("failed to get client address from request".to_string()))
    }

    fn get_current_hash(&self) -> Result<ByteArray32, Error> {
        let Some(last_hash) = self.plugin.btc.get_latest_hash() else {
            return Err(Error::status("failed to get latest hash", Status::NotFound));
        };

        last_hash.as_str().try_into()
            .map_err(|e| Error::other(format!("failed to parse latest hash, maybe mempool return malformed hash?, {last_hash}"), e))
    }

    fn get_path(&self) -> Result<String, Error> {
        self.ctx
            .get_http_request_path()
            .map_err(|s| Error::status("failed to get path", s))
    }

    fn get_timestamp(&self) -> Result<u64, Error> {
        self.get_header("X-PoW-Timestamp")?
            .parse()
            .map_err(|e| forbidden(format!("failed to parse timestamp: {}", e)))
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("failed to get timestamp")
        .as_secs()
}

impl HttpHook for Hook {
    fn filter_name() -> Option<&'static str> {
        Some("PoW")
    }

    async fn on_request_headers(
        &self,
        _num_headers: usize,
        _end_of_stream: bool,
    ) -> Result<(), impl Into<Response>> {
        let addr = self.get_client_address()?;
        let addr: SocketAddr = addr
            .parse()
            .map_err(|s| forbidden(format!("invalid client address {}: {}", s, addr)))?;
        if self
            .plugin
            .whitelist
            .iter()
            .any(|cidr| cidr.contains(addr.ip()))
        {
            return Ok(());
        }
        let host = self.get_header(":authority")?;
        let path = self.get_path()?;

        log::debug!("{} -> {}{}", addr, host, path);

        let Some(found) = self.plugin.router.matches(&host, &path) else {
            log::debug!("no matched route found, skip rate limit");
            return Ok(());
        };

        let key = format!(
            "{}:{}:{}{}",
            addr.ip(),
            found.rate_limit.current_bucket(),
            host,
            found.pattern()
        );
        let counter = self
            .plugin
            .counter_bucket
            .get(&key)
            .map_err(|s| Error::other("failed to get counter", s))?;
        let difficulty =
            counter / found.rate_limit.requests_per_unit as u64 * self.plugin.difficulty;
        let current = self.get_current_hash()?;
        log::debug!(
            "key: {}, counter: {}, difficulty: {}",
            key,
            counter,
            difficulty
        );

        if difficulty == 0 {
            self.plugin.counter_bucket.inc(&key, 1);
            return Ok(());
        }

        let target = get_difficulty(difficulty);

        let make_body = |error: &str| too_many_request(current, difficulty, error.to_string());

        let timestamp = self
            .get_timestamp()
            .map_err(|_| make_body("Missing X-PoW-Timestamp in header, or malformed"))?;

        if timestamp + 60 < now() {
            return Err(make_body("timestamp expired"));
        }

        let nonce = self
            .get_header("X-PoW-Nonce")
            .map_err(|_| make_body("Missing X-PoW-Nonce in header"))?;

        let nonce = hex::decode(nonce)
            .map_err(|s| make_body(&format!("X-PoW-Nonce must be a hex string: {}", s)))?;

        let last = self
            .get_header("X-PoW-Base")
            .map_err(|_| make_body("Missing X-PoW-Base in header"))?;

        if !self.plugin.btc.check_in_list(&last) {
            return Err(make_body("X-PoW-Base are expired, please use current"));
        }

        let last: ByteArray32 = last
            .as_str()
            .try_into()
            .map_err(|e| make_body(&format!("failed to parse X-PoW-Base hash: {}", e)))?;

        let mut data = last.as_bytes().to_vec();
        data.extend(timestamp.to_be_bytes());
        data.extend(path.as_bytes());

        if !valid_nonce(&data, target, &nonce) {
            return Err(make_body("Invalid nonce, maybe difficulty upgraded"));
        }

        self.plugin.counter_bucket.inc(&key, 1);
        Ok(())
    }
}

fn valid_nonce(data: &[u8], difficulty: ByteArray32, nonce: &[u8]) -> bool {
    let mut hasher = sha2::Sha256::new();
    hasher.update(data);
    hasher.update(nonce);
    let hash = hasher.finalize();
    let slice: &[u8; 32] = &hash.into();
    let target: ByteArray32 = slice.into();
    target <= difficulty
}

#[cfg(test)]
mod test {
    use crate::valid_nonce;
    use pow_types::bytearray32::ByteArray32;

    #[test]
    fn mine() {
        let last: ByteArray32 = "000000000000000000010915948e0d6b2c40aa4144ed4277f978e231f4c44732"
            .try_into()
            .expect("failed to parse last hash");
        // 000010c6f7a0b5edffffffffffffffffffffffffffffffffffffffffffffffff
        let difficulty: ByteArray32 =
            "000010c6f7a0b5edffffffffffffffffffffffffffffffffffffffffffffffff"
                .try_into()
                .expect("failed to parse difficulty");

        loop {
            let nonce = rand::random::<[u8; 8]>();
            if valid_nonce(last.as_bytes(), difficulty, &nonce) {
                print!("found nonce:");
                print_hex(&nonce);
                println!();
                break;
            }
        }
    }

    fn print_hex(bytes: &[u8]) {
        for byte in bytes {
            print!("{:02x}", byte);
        }
    }

    #[test]
    fn decode() {
        let nonce = "aaed9b41fcf6dc5";
        let hex = hex::decode(nonce).expect("invalid hex");
        print_hex(&hex);
    }
}
