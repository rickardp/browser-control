@0x803da837ec661407;

using Errors = import "errors.capnp";

# Schema version. Bumped on breaking changes.
# Client and daemon embed this; mismatch (major) -> graceful daemon respawn.
const schemaVersion :UInt32 = 1;

struct VersionInfo {
  schemaVersion @0 :UInt32;
  daemonVersion @1 :Text;   # cargo pkg version of running daemon
  browserKind   @2 :Text;   # "firefox" | "chromium" | "brave" | ...
  browserVersion @3 :Text;
}

# ---------- Lock-free operations ----------
# These do not change user-visible browser state. Implemented via the daemon's
# private scratch-tab pool with strict invariants.

struct FetchRequest {
  url @0 :Text;
  method @1 :Text;          # "GET" if empty
  headers @2 :List(Header);
  body @3 :Data;
  timeoutMs @4 :UInt32;     # 0 = default (30s)
}

struct Header {
  name @0 :Text;
  value @1 :Text;
}

struct FetchResponse {
  status @0 :UInt16;
  headers @1 :List(Header);
  body @2 :Data;
}

struct Cookie {
  name @0 :Text;
  value @1 :Text;
  domain @2 :Text;
  path @3 :Text;
  expiresEpochS @4 :Int64;  # -1 = session
  httpOnly @5 :Bool;
  secure @6 :Bool;
  sameSite @7 :Text;        # "strict" | "lax" | "none" | ""
}

interface LockFree {
  fetch @0 (req :FetchRequest) -> (result :Result);
  struct Result { union { ok @0 :FetchResponse; err @1 :Errors.Error; } }

  getCookies @1 (origin :Text) -> (result :CookieListResult);
  struct CookieListResult { union { ok @0 :CookieList; err @1 :Errors.Error; } }
  struct CookieList { cookies @0 :List(Cookie); }

  setCookies @2 (cookies :List(Cookie)) -> (result :UnitResult);
  struct UnitResult { union { ok @0 :Unit; err @1 :Errors.Error; } }
  struct Unit {}
}

# ---------- Locked operations ----------
# Hold an exclusive lock on user-visible browser state. The lock is the
# capability lifetime: drop the LockedSession and the lock releases.

struct Target {
  id @0 :Text;
  url @1 :Text;
  title @2 :Text;
  type @3 :Text;            # "page" | "iframe" | ...
}

struct EvalRequest {
  targetId @0 :Text;
  expression @1 :Text;
  awaitPromise @2 :Bool;
  timeoutMs @3 :UInt32;
}

struct EvalResponse {
  json @0 :Text;            # serialized RemoteObject result
}

struct ScreenshotRequest {
  targetId @0 :Text;
  format @1 :Text;          # "png" | "jpeg"
  fullPage @2 :Bool;
}

struct ScreenshotResponse {
  bytes @0 :Data;
  format @1 :Text;
}

struct NavigateRequest {
  targetId @0 :Text;
  url @1 :Text;
  waitUntil @2 :Text;       # "load" | "domcontentloaded" | "networkidle"
  timeoutMs @3 :UInt32;
}

# Event delivered to a Listener for active subscriptions on the held lock.
struct Event {
  json @0 :Text;            # opaque envelope, engine-tagged
}

interface Listener {
  pushEvent @0 (event :Event) -> ();
}

interface LockedSession {
  getTree @0 () -> (result :TreeResult);
  struct TreeResult { union { ok @0 :Tree; err @1 :Errors.Error; } }
  struct Tree { targets @0 :List(Target); }

  eval @1 (req :EvalRequest) -> (result :EvalResultEnv);
  struct EvalResultEnv { union { ok @0 :EvalResponse; err @1 :Errors.Error; } }

  screenshot @2 (req :ScreenshotRequest) -> (result :ScreenshotResultEnv);
  struct ScreenshotResultEnv { union { ok @0 :ScreenshotResponse; err @1 :Errors.Error; } }

  navigate @3 (req :NavigateRequest) -> (result :Unit);
  struct Unit { union { ok @0 :Empty; err @1 :Errors.Error; } }
  struct Empty {}

  subscribe @4 (topics :List(Text), listener :Listener) -> (result :SubResult);
  struct SubResult { union { ok @0 :Empty; err @1 :Errors.Error; } }
}

# ---------- Health & diagnostics ----------

struct LatencyMs {
  p50 @0 :UInt32;
  p95 @1 :UInt32;
}

struct UpstreamHealth {
  engine @0 :Text;          # "bidi" | "cdp"
  latency @1 :LatencyMs;
  errors5min @2 :UInt32;
  sessionAgeS @3 :UInt32;
}

struct TabHealth {
  id @0 :Text;
  url @1 :Text;
  state @2 :Text;           # "responsive" | "degraded" | "stuck" | ...
  lastSeenMsAgo @3 :UInt32;
  daemonOwned @4 :Bool;
}

struct ClientInfo {
  name @0 :Text;
  pid @1 :UInt32;
  connectedS @2 :UInt32;
}

struct QueueInfo {
  active @0 :Text;          # name of current lock holder, or empty
  waiting @1 :List(Text);
}

struct HealthReport {
  healthy @0 :Bool;
  browser @1 :BrowserInfo;
  daemon @2 :DaemonInfo;
  upstream @3 :UpstreamHealth;
  tabs @4 :List(TabHealth);
  clients @5 :List(ClientInfo);
  queue @6 :QueueInfo;
  warnings @7 :List(Text);

  struct BrowserInfo {
    kind @0 :Text;
    version @1 :Text;
    pid @2 :UInt32;
    uptimeS @3 :UInt32;
  }

  struct DaemonInfo {
    pid @0 :UInt32;
    uptimeS @1 :UInt32;
    version @2 :Text;
  }
}

# ---------- Daemon root ----------
# The bootstrap capability. All RPC starts here.

interface Daemon {
  version @0 () -> (info :VersionInfo);
  health  @1 () -> (report :HealthReport);

  lockFree @2 () -> (cap :LockFree);

  # Acquire the exclusive lock. The returned capability is the lease;
  # drop it (or disconnect) to release. Blocks in FIFO until granted,
  # subject to per-call timeout.
  acquireLocked @3 (clientName :Text, timeoutMs :UInt32) -> (result :AcquireResult);
  struct AcquireResult { union {
    granted @0 :LockedSession;
    err @1 :Errors.Error;
  } }

  # Diagnostic dump of the in-memory TRACE ring buffer.
  diagnose @4 () -> (text :Text);
}
