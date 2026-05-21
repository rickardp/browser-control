@0xe4b95542d258db58;
# browser-control daemon error taxonomy.

enum ErrorCode {
  # Caller-side problems
  badRequest @0;
  # Recoverable transients
  browserStarting @1;
  lockBusy @2;
  lockQueueFull @3;
  sessionLost @4;
  daemonShuttingDown @5;
  timeout @6;
  tabRecovered @7;          # informational; rarely surfaced
  blockedOnDialog @8;
  # Non-recoverable / require user/agent intervention
  browserNotRunning @9;
  browserUnhealthy @10;
  browserGone @11;
  tabHung @12;
  originUnreachable @13;
  internal @14;
}

struct Error {
  code @0 :ErrorCode;
  message @1 :Text;
  # Imperative short hint for agents (e.g. "retry in 500ms", "tab-reload required").
  hint @2 :Text;
  # True if the caller may safely retry the same request unchanged.
  recoverable @3 :Bool = false;
  # Structured machine-readable extras (opaque to the wire layer).
  details @4 :Text;
}
