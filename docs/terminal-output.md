# Terminal output

This inventories Waywarp-owned output in the executable and installer, including
messages normally hidden in an instance log or behind `WAYWARP_LOG=debug`.
Placeholders stand for runtime values. Native OS/library errors and `warp-cli`
output are unbounded; their entry points are listed rather than inventing a finite
list of their possible text. Test fixtures, Cargo/Nix build output, and CI runner
messages are not application output.

## What was hard to read

| Problem | User impact | Refactor |
| --- | --- | --- |
| A status was one comma-separated sentence containing identity, access, five location fields, NAT, relay, retries, and health. | Important failures appeared at the end of a line several terminal widths long. | Compact, aligned rows with separate `Instance` and `Status` fields. A location mismatch is beside the status. |
| Progress was an arbitrary string with `…` appended indiscriminately. | Even `connecting ... failed: ...` looked unfinished. There was no stable severity or instance context. | Typed stages and failures through tracing; `info:` and `warning:` labels on stderr; structured instance context in detached logs rather than console prefixes. |
| Every successful relay ping was normal progress. | A large Mudfish selection drowned out the actual connection attempts. | Probe success/failure details are debug diagnostics. Real connection failures remain warnings. |
| Connection exhaustion joined three failures with semicolons inside parentheses. | The user had to untangle one very long error. | Count, recent routes on separate lines, and an explicit notice when earlier failures are omitted. |
| Errors crossed IPC as `{error:#}` strings. | Cause boundaries disappeared before the client could format them. | Serializable `Failure { message, causes }`; restore the chain at the receiving client. |
| Rust's `main -> anyhow::Result` formatter and tracing produced different diagnostic styles. | `Error:`, numbered causes, tracing metadata, and bare progress appeared together. | One terminal renderer for diagnostics, fields, and causes. Clap retains ownership of usage errors. |
| Foreground readiness was logged and printed; fatal errors were also logged and returned. | Duplicate status/failure messages; foreground failures suggested a log file that was not being written. | One foreground readiness block and one final error; only detached startup points to an instance log. |
| `println!` handled command output. | Closing a pipe could produce a broken-pipe panic instead of normal command termination. | Fallible writes and explicit exit codes; only stdout broken pipes are treated as successful termination. |
| ANSI was enabled solely by terminal detection. | `NO_COLOR` and dumb terminals were ignored; remote labels could inject terminal controls. | Per-stream color policy and escaped control characters in human output. JSON and native passthrough are untouched. |
| Help was not wrapped. | Long option descriptions and introductory paragraphs ran off narrow terminals. | Enable the existing Clap `wrap_help` feature, with an 80-column maximum. |
| Installer progress was on stdout and missing-tool advice repeated for each tool. | Mixed result/progress output and repetitive warnings. | Progress/diagnostics on stderr; one completion on stdout; one grouped missing-tools warning. |

## Command results and stream contracts

| Entry point | Output | Stream / exit |
| --- | --- | --- |
| `up proxy`, `up bridge` | Status block; detached startup also shows `Log`. | stdout; success 0, runtime failure 1, missing registration consent 2 |
| `up ... --foreground` | One readiness block; lifecycle diagnostics continue separately. | readiness stdout; diagnostics stderr |
| `status [INSTANCE]` | One block per responding instance, separated by a blank line. | stdout; 1 if any returned status is disconnected/degraded/unknown/unable/connecting or location-mismatched |
| Empty human `status` | `no running instances` | stdout; 0 |
| `status --json` | The existing compact JSON object per instance, newline-delimited. No headers, captions, color, or empty-result message. | stdout; same health exit status as human output; errors only on stderr |
| `down` | `info: stopped` | stderr via tracing; 0 after supervisor EOF confirms shutdown |
| `import` | `info: imported registration`, then an indented `from:` field | stderr via tracing; 0 after import completes |
| `warp-cli` | The native command's stdout/stderr descriptors are forwarded unchanged, including output without a trailing newline. | both streams, native exit code; Waywarp adds nothing on success |
| `-h`, `--help`, `help`, subcommand help | Clap-generated usage, descriptions, arguments, options, defaults and possible values. | stdout; 0 |
| `-V`, `--version` | `waywarp VERSION` | stdout; 0 |
| Syntax/value errors and missing consent | Clap `error:` text, usage/suggestions where applicable. Consent text has separate review-link and action lines and ends with a newline. | stderr; 2 |
| Other command failures | `error: MESSAGE`, indented `caused by:` entries; multiline helper text stays indented. | stderr; 1 |

The status renderer is `src/output.rs`. Callers are `src/client.rs` and
`supervise::Reporter::up`. `Status` no longer owns human presentation through a
`Display` implementation. Its serialized fields are unchanged.

### Every human status component

Rows have no leading indentation or label colons, and their values share one
column. There are no blank lines within a block. Continuation values use the same
column with an empty label.

- `Instance`: `INDEX (NAME)`, or just `INDEX` when unnamed. Both the key and value
  are cyan when stdout color is enabled.
- `Status`: `Unknown`, `Disconnected`, `Unable to connect`, `Connecting`,
  `Degraded`, or `Connected`. The value is green when healthy and yellow
  otherwise; `(location mismatched)` is appended in red when `matched` is false.
- `Access`: `ADDRESS (proxy)` or `LINK (bridge)`.
- Bridge-only `Link4`, `Link6`: host address/prefix and namespace gateway.
- `Edge`: the WARP edge colo, or `Unavailable`.
- `Bootstrap`: the relay label, or `Direct`.
- `Geo4`, `Geo6`: geofeed country and optional city, or `Unavailable`.
- `Probe4`, `Probe6`: observed IP followed by `(COLO)`, or `Unavailable`.
- Bridge-only `NAT`: `Auto`, `Always`, or `Never`.
- `SNAT4`, `SNAT6`: verified targets or `blocked (no verified address)`;
  omitted when source NAT is disabled with a valid registration.
- `Routed`: one canonical network per line, or `None`.
- Invalid NAT configuration: `Warning` reports unreadable registration and
  removed exemptions, followed by `using last verified addresses`.
- Nonzero `Retries`: `1 rebootstrap` or `COUNT rebootstraps`.
- Detached `up` only: a final `Log` row with the log path.

Example detached startup result, without color:

```text
Instance   2 (tokyo)
Status     Connected
Access     127.0.0.1:1082 (proxy)
Edge       NRT
Bootstrap  Direct
Geo4       JP/Tokyo
Geo6       Unavailable
Probe4     203.0.113.4 (NRT)
Probe6     Unavailable
Log        /path/to/waywarp/2/waywarp.log
```

`Probe` shows what the request observed; `Geo` shows the corresponding geofeed
classification. A missing observation is not fabricated. Health requires a
currently verified connection and matching locations; NAT validity does not
change exit-code semantics.

## Every setup/rebootstrap event

Events originate in `src/supervise/mod.rs` and `src/supervise/bootstrap.rs`, cross
the setup channel as `Progress`, and render/log in `src/output.rs`. Rebootstrap
uses the same events.

| Event | Console stderr | Log level / extra fields |
| --- | --- | --- |
| `Registering` | `info: registering WARP` | info |
| `StartingDaemon` | `info: starting warp-svc` | info |
| `Connecting` | `info: connecting directly` or `info: connecting via RELAY` | info |
| `Migrating` | `info: moving tunnel to direct path` | info |
| `CheckingLocations` | `info: checking geo and probe` | info |
| `AttemptFailed` | `warning: failed to connect ROUTE`, then `error:` and `causes:` fields | warn; error, causes |
| `RelayProbed` | `debug: waywarp::lifecycle: relay probe succeeded`, when enabled | debug; relay, millis, optional colo |
| `RelayProbeFailed` | `debug: waywarp::lifecycle: relay probe failed`, when enabled | debug; relay, error, causes |

Lifecycle events use the `waywarp::lifecycle` tracing target and retain structured
`instance` and optional `instance_name` fields in detached logs. Console output
omits those fields and has no instance prefix. Ordinary message words are
lowercase; identifiers and acronyms such as WARP are preserved.

Commands default to info for lifecycle events and warn for other diagnostics.
Foreground and detached supervisors default to info. These defaults apply to
redirected stderr too; an explicit `WAYWARP_LOG` filter controls visibility,
including lifecycle notices. Status and JSON results are independent of the log
filter. No cursor movement, spinners, unconditional ellipses, or decorative
separators are used.

## Every tracing diagnostic

All of these can be terminal-visible in a foreground run or with an appropriate
`WAYWARP_LOG` filter. Sources retain structured fields. Commands and foreground
runs use the same tracing console formatter, whether stderr is a terminal or
redirected: lowercase severity, indented fields, no timestamp/thread clutter,
and no `instance` or `instance_name` fields. Debug/trace records retain their
target. Detached logs retain timestamps, uppercase levels, targets, thread names,
and structured instance context, with ANSI disabled.

Progress records are listed above; the remaining tracing sites are:

| Source | Level | Message and extra fields |
| --- | --- | --- |
| `src/output.rs` | info | `stopped` (instance); command confirmation |
| same | info | `imported registration` (instance, from) |
| `src/supervise/mod.rs` | info | `starting instance` (instance, optional instance_name, access) |
| same | info | `instance ready` (instance, optional instance_name, compact status); detached only |
| same | info | `stopped` (instance, optional instance_name) |
| same | info | `stopping` (signal) |
| same | warn | `geo fields are unavailable: ERROR` |
| same | warn | `setup abandoned by the client` |
| same | error | `setup failed: ERROR`; detached setup only |
| `src/supervise/bootstrap.rs` | info | `rebootstrapped` (elapsed) |
| same | warn | `location probe failed: ERROR` (family) |
| same | warn | `reading locations failed; verification remains pending: ERROR` |
| same | warn | `location no longer matches: ERROR` |
| `src/supervise/bridge.rs` | info | `updated bridge NAT (existing connections keep their NAT mapping)` (mode, v4, v6, routed) |
| same | warn | `bridge configuration unavailable or invalid; removing routed exemptions` |
| same | warn | `bridge reconciliation will retry: ERROR` |
| `src/supervise/control.rs` | warn | `cannot serve a control client` (error) |
| `src/warp/daemon.rs` | info | `started warp-svc` (pid) |
| same | debug | `warp-svc is ready` (elapsed) |
| `src/warp/monitor.rs` | info | `WARP state changed` (state) |
| same | debug | `unrecognized WARP status` (native line) |
| same | warn | `warp-cli status listener failed: ERROR` |
| `src/location/geofeed.rs` | debug | `refreshing` (URL) |
| same | warn | `using stale data: ERROR` (cache path) |
| same | warn | `Cloudflare's geofeed does not contain the address` (address) |
| `src/dataplane/observe.rs` | debug | `observed WARP QUIC activity` (attempts, exchanges) |
| `src/dataplane/mod.rs` | warn | `accepting a TCP connection failed` (error) |
| same | debug | `TCP connection failed` (error) |
| `src/dataplane/tcp.rs` | debug | `TCP splice closed` (error) |
| `src/dataplane/udp.rs` | warn | `cannot register a UDP flow` (error) |
| same | warn | `cannot open a UDP socket` (destination, error) |
| same | warn | `cannot register a prepared UDP association` (error) |
| same | debug | `SOCKS5 association failed` (destination, error) |
| same | debug | `UDP flow failed` (destination, error) |
| same | debug | `UDP send failed` (destination, error) |
| `src/tool.rs` | debug | `running` (program, arguments) |
| `src/notify.rs` | warn | `cannot notify systemd` (error) |

The old separate `instance failed: ERROR` tracing message is removed; final
runtime errors are rendered once at the entry point. Systemd readiness also gets
a compact, single-line status through its notification socket, not another
terminal status block.

## Every application-defined error family

These are not independent print sites. They become a final error, a typed attempt
failure, or one of the diagnostic records above. Native failures propagated by
`?` contribute their own cause text. Parser errors are wrapped by Clap; other
errors are wrapped by the output renderer.

### Argument/value validation

- `src/store.rs`: invalid instance names (length/allowed characters/leading
  letter); instance indices outside 0–255.
- `src/cli.rs`: `expected an IPv4 loopback address with a port, such as
  127.0.0.1:1080`; Clap's built-in socket/IP/port/NAT-mode validation.
- `src/bridge.rs`: `VALUE is not an IPv4 network`, `VALUE is not an IPv6 network`,
  `SUBNET is not a canonical /30 network`, `SUBNET is not a canonical /126 network`.
- `src/location/mod.rs`: invalid place/country code with country/city examples;
  missing city after slash; invalid three-letter colo; malformed `FIELD=VALUE`
  constraint; missing family suffix; unknown location field; repeated field.
- `src/via/mod.rs`: `direct takes no value`; SOCKS5 credentials must use environment
  variables; SOCKS5 requires IPv4 `ADDRESS:PORT`; unknown via kind with supported
  alternatives; `--via direct may appear only once`.
- `src/via/mudfish.rs`: invalid node id; unknown filter field with supported fields;
  filter terms must be letters/digits; `empty Mudfish filter term`.
- `src/warp/mod.rs`: new registration requires explicit Cloudflare consent;
  review URL and `--accept-tos`/import alternatives.

### Store and command failures

- `src/store.rs`: `HOME is not set`; `XDG_RUNTIME_DIR is not set`; XDG paths must
  be absolute; no instance with a name (includes assignment command); name belongs
  to another instance; instance running/starting (includes stop command); `reading
  PATH`; Team registration lacks an IPv4 edge on the requested port; existing
  registration requires `--replace`; source contains no registration; source is
  not a regular file; `copying PATH`.
- `src/client.rs`: bridge requires root/sudo; existing host link; cannot listen
  (includes `--listen` advice); foreground `instance INDEX failed`; detached
  `instance INDEX could not start` plus log path; `starting the supervisor`;
  `supervisor exited during setup`; instance not running (unprivileged callers
  get the root-owned-instance/sudo hint); `unexpected supervisor response`;
  `instance did not stop within 15 s`.
- `src/output.rs`: `cannot write command output`, with the underlying write/flush
  error. A stdout broken pipe is deliberately silent and exits successfully.

### Isolation, bridge, IPC and supervision failures

- `src/sandbox.rs`: `bind-mounting SOURCE onto TARGET`; helper not on PATH;
  `adding ip and nft to /usr/sbin`; required WARP path absent (create as root or
  start WARP once); creating user/mount/network namespaces; entering the private
  network namespace; `hiding the host's nscd`; `redirecting TCP`; opening
  `/dev/net/tun`; `creating the TUN`; `private namespace task panicked`.
- `src/bridge.rs`: overlapping host route (includes the appropriate subnet flag);
  `creating host link LINK`; `loading the bridge firewall`; `WARP's link is absent`;
  `setting the MTU of LINK to MTU`.
- `src/bridge/watch.rs`: `subscribing to WARP link changes`; `reading WARP link
  changes`.
- `src/ipc.rs`: `IPC message is too large`; `peer process is gone`; `truncated IPC
  message`; `too many IPC descriptors`; `peer process exited`; wrong descriptor
  count. JSON decoding and socket/descriptor failures add native causes.
- `src/supervise/mod.rs`: `instance setup was cancelled`; `WARP changed before the
  instance became ready`; `reconciling the bridge with WARP`; wrong setup
  descriptor count; missing instance lock; `binding the control socket`; `loading
  Cloudflare's location data`; missing proxy listener; data plane stopped/failed;
  `warp-svc exited (STATUS)`.
- `src/supervise/bridge.rs`: `bridge stopped`; `watching bridge configuration`;
  `reconciling bridge`.
- `src/supervise/control.rs`: `accepting a control client`; `running warp-cli`.
- `src/supervise/bootstrap.rs`: connection exhaustion with attempt count, up to
  three recent failures and omission notice; `bootstrap was cancelled`; WARP
  changed before verification could be published, while restoring the bridge,
  or while verifying the tunnel; neither location probe succeeded; data plane
  stopped during verification; `verifying a stable WARP tunnel`; `tunnel
  verification timed out`; `IPv4 location probe panicked`; `rebootstrap failed`.

### WARP, locations, relays and networking failures

- `src/warp/daemon.rs`: `starting warp-svc`; daemon startup timeout; owner thread
  failed; daemon exited during startup/registration; `registering a WARP device`.
- `src/warp/monitor.rs`: `starting warp-cli --listen`; `warp-cli stdout`.
- `src/warp/mod.rs`: switching WARP mode; non-MASQUE tunnel; missing edge colo;
  proxy port not accepting connections; disconnect timeout (last state);
  connection timeout (last state); `WARP connection was cancelled`.
- `src/location/mod.rs`: unknown Cloudflare colo; no geofeed address in requested
  place; `FIELD is ACTUAL, not EXPECTED` (also covers unavailable observations).
- `src/location/geofeed.rs`: `Cloudflare returned incomplete location data`.
- `src/location/probe.rs`: wrong IP family returned by probe; missing colo or IP;
  IP parsing and response UTF-8 errors.
- `src/http.rs`: `fetching URL`, plus HTTP/TLS/body-limit/native errors.
- `src/via/mod.rs`: credential byte limit; both username/password environment
  variables required; no IPv4 Mudfish node matches the filter.
- `src/via/mudfish.rs`: `fetching the Mudfish node list`, plus JSON decoding errors.
- `src/via/interface.rs`: no route to destination; no local route; route uses a
  virtual link (physical-interface advice and candidates); interface does not
  exist; `binding a socket to INTERFACE`.
- `src/via/auth_limit.rs`: reading the pacing clock; opening/locking authentication
  limiter; invalid authentication timestamp.
- `src/via/socks5.rs`: connecting to relay; rejected authentication method;
  rejected credentials; refused operation with reply code; unsupported address
  type; UDP port zero.
- `src/via/ping.rs`: edge did not answer over UDP; trace response lacked a colo.
- `src/via/transport.rs`: cancelled or timed-out relay socket operations, plus
  native socket/read/write errors.
- `src/dataplane/mod.rs`: `waiting for the data plane`; `installing the connection
  route`; `installing the direct path`; data plane stopped; `reading the private
  uplink`; `writing the private uplink`.
- `src/dataplane/tcp.rs`: `too many TCP connections`; native socket/read/write
  failures are diagnostic causes, including write-zero.
- `src/tool.rs`: `running PROGRAM`; helper stdin/stdout/stderr and pipe errors;
  `PROGRAM was cancelled`; `PROGRAM ARGUMENTS timed out`; output exceeds 16 MiB;
  `PROGRAM ARGUMENTS failed (EXIT STATUS)`, with nonempty native stderr as a
  separate, possibly multiline cause. Empty stderr still leaves the exit status
  visible.

## Installer and external-output boundaries

`install.sh` uses a small POSIX `report` helper rather than depending on the Rust
binary before installation:

- stderr `step: downloading waywarp-SYSTEM (VERSION)`.
- stderr optional `note: verified build provenance`.
- stdout `note: installed waywarp VERSION to TARGET`.
- stderr grouped `warning: not on PATH: TOOLS`, then one dependency-advice note.
- stderr `error: waywarp install: REASON`, exit 1, for: non-Linux host;
  unsupported architecture/build-from-source advice; missing curl/wget; failed
  binary download; failed checksum-file download; checksum mismatch; provenance
  verification failure; unwritable prefix/use `WAYWARP_PREFIX` advice.
- Native curl/wget, `gh attestation verify`, `install`, checksum, and other shell
  utility failures can also supply stderr. They are not reworded by Waywarp.

Other boundaries:

- `warp-cli` passthrough intentionally retains the native application's formatting,
  color, bytes and exit status.
- Internal `ip`, `nft`, and `warp-cli` stdout is captured, not dumped to the user;
  failures enter the error renderer through `src/tool.rs`.
- Managed `warp-svc` stdout/stderr is suppressed. Its own files are not terminal
  output. The status listener captures native JSON stdout and suppresses stderr.
- Runtime panics are Rust crash reports, not normal user diagnostics.
- Listing all instances skips supervisors that stopped during the query, not
  arbitrary errors. Malformed responses, permission errors, and timeouts fail the
  command. Selecting an instance directly also surfaces shutdown errors.

## Why a small framework, not another presentation crate

The missing abstraction was ownership and meaning, not a spinner or a table.
`src/output.rs` owns human status, final errors, stdout writes, severity, color
and escaping; `src/output/logging.rs` formats tracing events for the console or
detached logs. Lifecycle notices go through tracing, while status and JSON use
explicit writes with write-error handling. `Progress` and `Failure` preserve
meaning across IPC, and the entry point owns final rendering and exit codes. No
extra animation thread or terminal UI lifecycle is needed for supervisors.

Clap already owns usage/help and tracing already owns diagnostic filtering and
service logs. Reuse them; enable Clap's wrapping support instead of hand-writing
another word wrapper. Its terminal-size support adds only transitive dependencies,
not a dedicated output framework dependency.

Human status/import/down text intentionally changes. Scripts should use the
unchanged `status --json` contract. Legacy string failures from older supervisors
remain readable; new failures carry structured causes. Nonempty `NO_COLOR`,
`TERM=dumb`, and redirected streams suppress framework color independently for
stdout and stderr.

Renderer/protocol tests cover layouts, health, NAT, causes, escaping, color,
JSON and broken pipes. `tests/integration/` exercises real command streams,
exit codes, consent newlines, log filtering and descriptor passthrough against
isolated fixtures; it skips store-dependent cases as root rather than touching
real system paths. The Nix installer and VM checks cover the shell and actual
supervisor/service paths.
