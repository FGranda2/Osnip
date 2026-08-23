// Osnip — pure logic shared by BarWidget.qml and Panel.qml.
//
// Everything here is deliberately free of QML types so it can be run
// under `node --test` (see tests/model.test.js). The rule of thumb: if
// it decides something, it belongs here; if it draws something, it
// belongs in the .qml. Plugins run unsandboxed inside the long-lived
// omarchy-shell process, so a logic bug that throws would degrade the
// user's whole bar — which is exactly the code worth unit-testing.

var PLUGIN_ID = "io.github.franciscogranda.osnip"

// Protocol version this panel is written against. The daemon announces
// its own in the `hello` line; see osnip's crates/osnip-core.
var PROTOCOL_VERSION = 2

// ---------------------------------------------------------------- paths

// The daemon binds $XDG_RUNTIME_DIR/osnip.sock. Keep the fallback
// in sync with osnip_core::default_socket_path.
function socketPath(runtimeDir) {
  var dir = String(runtimeDir || "")
  if (dir === "") return ""
  return dir.replace(/\/+$/, "") + "/osnip.sock"
}

// Qt caches Image sources by URL, and a rotate or flip rewrites a
// thumbnail without changing its path — so the revision has to ride
// along in the query string or the panel keeps showing stale pixels.
function thumbnailUrl(pin) {
  if (!pin || !pin.thumbnail) return ""
  return "file://" + String(pin.thumbnail) + "?rev=" + Number(pin.revision || 0)
}

// ------------------------------------------------------------- requests

function subscribeRequest() { return JSON.stringify({ kind: "subscribe" }) }
function listRequest() { return JSON.stringify({ kind: "list" }) }
function captureRequest() { return JSON.stringify({ kind: "capture" }) }
function clipboardRequest() { return JSON.stringify({ kind: "clipboard" }) }
function closeAllRequest() { return JSON.stringify({ kind: "close_all" }) }

function closeRequest(id) {
  return JSON.stringify({ kind: "close", id: Number(id) })
}

// `action` must be one of the snake_case spellings the daemon's
// PinActionKind serializes to; PIN_ACTIONS is the whole vocabulary.
function pinActionRequest(id, action) {
  return JSON.stringify({ kind: "pin_action", id: Number(id), action: String(action) })
}

var PIN_ACTIONS = ["copy", "save", "rotate_right", "rotate_left", "flip_h", "flip_v"]

function isPinAction(action) {
  return PIN_ACTIONS.indexOf(String(action)) !== -1
}

// ------------------------------------------------------------- decoding

// Never throws. A malformed line is the daemon's problem to fix, not a
// reason to take the bar down with an uncaught exception.
function parseLine(raw) {
  var text = String(raw === undefined || raw === null ? "" : raw)
  text = text.replace(/^\s+|\s+$/g, "")
  if (text === "") return null
  try {
    var value = JSON.parse(text)
    if (!value || typeof value !== "object" || Array.isArray(value)) return null
    if (typeof value.kind !== "string") return null
    return value
  } catch (e) {
    return null
  }
}

function emptyState() {
  return {
    connected: false,
    protocol: 0,
    version: "",
    capabilities: [],
    pins: [],
    lastError: ""
  }
}

function hasCapability(state, name) {
  if (!state || !state.capabilities) return false
  return state.capabilities.indexOf(String(name)) !== -1
}

// Fold one decoded message into the panel's state, returning a new
// object rather than mutating — QML property bindings only re-evaluate
// when the property is reassigned.
//
// `hello`, `event`, and the four IpcResponse kinds all arrive on one
// stream and share the `kind` discriminator; the daemon guarantees they
// never collide.
function applyMessage(state, message) {
  var next = cloneState(state)
  if (!message) return next

  switch (message.kind) {
  case "hello":
    next.connected = true
    next.protocol = Number(message.protocol || 0)
    next.version = String(message.version || "")
    next.capabilities = Array.isArray(message.capabilities)
      ? message.capabilities.map(String)
      : []
    next.lastError = ""
    return next

  case "event":
    if (message.event === "pins_changed") next.pins = sanitizePins(message.pins)
    return next

  // The panel renders from events, so a `pins` reply is only a
  // fallback for the pre-subscribe window.
  case "pins":
    if (message.data && Array.isArray(message.data.pins)) {
      next.pins = sanitizePins(message.data.pins)
    }
    return next

  case "error":
    next.lastError = errorMessage(message.data)
    return next

  case "ok":
  case "pinned":
    next.lastError = ""
    return next

  default:
    return next
  }
}

function cloneState(state) {
  var base = state || emptyState()
  return {
    connected: base.connected === true,
    protocol: Number(base.protocol || 0),
    version: String(base.version || ""),
    capabilities: (base.capabilities || []).slice(),
    pins: (base.pins || []).slice(),
    lastError: String(base.lastError || "")
  }
}

// Drop anything malformed rather than letting it reach a delegate and
// throw mid-render. Sorted by id so the grid keeps a stable order
// instead of reshuffling on every event.
function sanitizePins(pins) {
  if (!Array.isArray(pins)) return []
  var out = []
  for (var i = 0; i < pins.length; i++) {
    var p = pins[i]
    if (!p || typeof p !== "object") continue
    var id = Number(p.id)
    if (!isFinite(id) || id <= 0) continue
    out.push({
      id: id,
      width: Math.max(0, Number(p.width) || 0),
      height: Math.max(0, Number(p.height) || 0),
      createdAt: Number(p.created_at_unix_ms) || 0,
      thumbnail: p.thumbnail ? String(p.thumbnail) : "",
      revision: Number(p.revision) || 0
    })
  }
  out.sort(function (a, b) { return a.id - b.id })
  return out
}

// IpcError is `{"kind": "...", ...}` with per-variant fields; surface
// the human-facing string where there is one.
function errorMessage(data) {
  if (!data || typeof data !== "object") return "Unknown daemon error"
  if (typeof data.message === "string" && data.message !== "") return data.message
  var kind = String(data.kind || "")
  switch (kind) {
  case "capture_canceled": return ""
  case "clipboard_no_image": return "The clipboard does not contain an image"
  case "protocol_mismatch":
    return "Protocol mismatch: panel speaks v" + PROTOCOL_VERSION
      + ", daemon speaks v" + Number(data.daemon || 0)
  default: return kind === "" ? "Unknown daemon error" : kind.replace(/_/g, " ")
  }
}

// A daemon older than the panel has no `pin_action` and no thumbnails;
// say so once rather than presenting buttons that quietly do nothing.
function needsBackendUpgrade(state) {
  return state.connected === true && Number(state.protocol) < PROTOCOL_VERSION
}

// ------------------------------------------------------------ presentation

function pinCount(state) {
  return state && state.pins ? state.pins.length : 0
}

function countLabel(count) {
  var n = Number(count) || 0
  if (n <= 0) return ""
  return n > 99 ? "99+" : String(n)
}

function tooltipText(state) {
  if (!state || !state.connected) return "Osnip · not running"
  var n = pinCount(state)
  if (n === 0) return "Osnip · no pins"
  return "Osnip · " + n + (n === 1 ? " pin" : " pins")
}

function sizeLabel(pin) {
  if (!pin) return ""
  return String(pin.width) + "×" + String(pin.height)
}

// Compact age for a grid caption: "now", "4m", "2h", "3d".
function ageLabel(nowMs, createdAtMs) {
  var delta = Number(nowMs) - Number(createdAtMs)
  if (!isFinite(delta) || delta < 0) return "now"
  var seconds = Math.floor(delta / 1000)
  if (seconds < 60) return "now"
  var minutes = Math.floor(seconds / 60)
  if (minutes < 60) return minutes + "m"
  var hours = Math.floor(minutes / 60)
  if (hours < 24) return hours + "h"
  return Math.floor(hours / 24) + "d"
}

// Fit the grid to the panel: honor the user's column count, but never
// ask for more columns than there are pins.
function gridColumns(requested, pinTotal) {
  var wanted = Math.round(Number(requested))
  if (!isFinite(wanted) || wanted < 1) wanted = 3
  if (wanted > 5) wanted = 5
  var total = Number(pinTotal) || 0
  if (total > 0 && total < wanted) return total
  return wanted
}

// Which cell the keyboard cursor lands on after a move. Clamped rather
// than wrapped, so holding an arrow key parks at an edge instead of
// cycling forever.
function moveCursor(index, delta, total) {
  var count = Number(total) || 0
  if (count <= 0) return 0
  var next = (Number(index) || 0) + (Number(delta) || 0)
  if (next < 0) return 0
  if (next > count - 1) return count - 1
  return next
}

// ---------------------------------------------------------------- install

// The AUR package that carries `osnip` and `osnip-daemon`.
var BACKEND_PACKAGE = "osnip"

function installSentinelPaths() {
  return {
    complete: "$XDG_RUNTIME_DIR/osnip-panel-install.complete",
    failed: "$XDG_RUNTIME_DIR/osnip-panel-install.failed"
  }
}

// Installing has to happen in a terminal the user can see: it runs
// pacman/AUR helpers that prompt, and a plugin must never acquire
// privileges invisibly inside omarchy-shell.
//
// The sentinel files exist because a presented terminal gives us no
// exit status — without them the panel could not tell "finished" from
// "user closed the window" and would spin forever.
function installCommand() {
  var s = installSentinelPaths()
  return 'rm -f "' + s.failed + '" "' + s.complete + '"; status=0; '
    + "omarchy pkg aur add " + BACKEND_PACKAGE + " || status=$?; "
    + 'if (( status == 0 )); then : > "' + s.complete + '"; '
    + "else printf '%s\\n' \"$status\" > \"" + s.failed + '"; fi; (exit "$status")'
}

function installProcessArgs() {
  return ["omarchy", "launch", "floating", "terminal", "with", "presentation", installCommand()]
}

// `which` is the cheapest way to answer "is the backend installed?"
// without running the binary itself.
function backendProbeArgs() {
  return ["which", "osnip"]
}

// Capture and clipboard go through the CLI rather than the socket so
// they work even when no daemon is running yet — the CLI auto-spawns
// one. Everything else in the panel needs a live connection anyway.
function cliArgs(subcommand) {
  return ["osnip", String(subcommand)]
}

if (typeof module !== "undefined") {
  module.exports = {
    PLUGIN_ID: PLUGIN_ID,
    PROTOCOL_VERSION: PROTOCOL_VERSION,
    PIN_ACTIONS: PIN_ACTIONS,
    BACKEND_PACKAGE: BACKEND_PACKAGE,
    socketPath: socketPath,
    thumbnailUrl: thumbnailUrl,
    subscribeRequest: subscribeRequest,
    listRequest: listRequest,
    captureRequest: captureRequest,
    clipboardRequest: clipboardRequest,
    closeAllRequest: closeAllRequest,
    closeRequest: closeRequest,
    pinActionRequest: pinActionRequest,
    isPinAction: isPinAction,
    parseLine: parseLine,
    emptyState: emptyState,
    hasCapability: hasCapability,
    applyMessage: applyMessage,
    sanitizePins: sanitizePins,
    errorMessage: errorMessage,
    needsBackendUpgrade: needsBackendUpgrade,
    pinCount: pinCount,
    countLabel: countLabel,
    tooltipText: tooltipText,
    sizeLabel: sizeLabel,
    ageLabel: ageLabel,
    gridColumns: gridColumns,
    moveCursor: moveCursor,
    installSentinelPaths: installSentinelPaths,
    installCommand: installCommand,
    installProcessArgs: installProcessArgs,
    backendProbeArgs: backendProbeArgs,
    cliArgs: cliArgs
  }
}
