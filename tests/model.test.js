const test = require("node:test")
const assert = require("node:assert/strict")
const fs = require("node:fs")
const path = require("node:path")
const Model = require("../Model.js")

const read = (name) => fs.readFileSync(path.join(__dirname, "..", name), "utf8")

// ---------------------------------------------------------------- wire

test("requests match the daemon's IpcRequest spellings", () => {
  // These strings are the contract with osnip-core's serde tags.
  // A typo here fails silently at runtime as a `bad_request` reply, so
  // pin the exact bytes.
  assert.equal(Model.subscribeRequest(), '{"kind":"subscribe"}')
  assert.equal(Model.listRequest(), '{"kind":"list"}')
  assert.equal(Model.captureRequest(), '{"kind":"capture"}')
  assert.equal(Model.clipboardRequest(), '{"kind":"clipboard"}')
  assert.equal(Model.closeAllRequest(), '{"kind":"close_all"}')
  assert.equal(Model.closeRequest(7), '{"kind":"close","id":7}')
  assert.equal(
    Model.pinActionRequest(3, "rotate_right"),
    '{"kind":"pin_action","id":3,"action":"rotate_right"}'
  )
})

test("pin action vocabulary matches PinActionKind exactly", () => {
  assert.deepEqual(Model.PIN_ACTIONS, [
    "copy", "save", "rotate_right", "rotate_left", "flip_h", "flip_v"
  ])
  for (const action of Model.PIN_ACTIONS) assert.ok(Model.isPinAction(action))
  assert.ok(!Model.isPinAction("rotate"))
  assert.ok(!Model.isPinAction(""))
})

test("every action the panel wires up is one the daemon accepts", () => {
  // Guards against a button being given a plausible-looking verb that
  // the daemon would reject.
  const qml = read("Panel.qml")
  const wired = [...qml.matchAll(/pinAction\([^,]+,\s*"([a-z_]+)"\)/g)].map((m) => m[1])
  assert.ok(wired.length > 0, "no pinAction call sites found in Panel.qml")
  for (const action of wired) {
    assert.ok(Model.isPinAction(action), `Panel.qml sends unknown action "${action}"`)
  }
})

test("socket path follows osnip_core::default_socket_path", () => {
  assert.equal(Model.socketPath("/run/user/1000"), "/run/user/1000/osnip.sock")
  assert.equal(Model.socketPath("/run/user/1000/"), "/run/user/1000/osnip.sock")
  assert.equal(Model.socketPath(""), "")
  assert.equal(Model.socketPath(null), "")
})

// ------------------------------------------------------------- parsing

test("parseLine never throws and rejects anything that is not a tagged object", () => {
  for (const bad of ["", "   ", "not json", "{unclosed", "[1,2]", "null", '"str"', "42", undefined, null]) {
    assert.equal(Model.parseLine(bad), null, `should reject ${JSON.stringify(bad)}`)
  }
  assert.deepEqual(Model.parseLine(' {"kind":"ok"} \n'), { kind: "ok" })
})

test("hello populates protocol, version, and capabilities", () => {
  const state = Model.applyMessage(Model.emptyState(), {
    kind: "hello",
    protocol: 2,
    version: "0.2.0",
    capabilities: ["subscribe", "pin_action", "thumbnails"]
  })
  assert.equal(state.connected, true)
  assert.equal(state.protocol, 2)
  assert.equal(state.version, "0.2.0")
  assert.ok(Model.hasCapability(state, "thumbnails"))
  assert.ok(!Model.hasCapability(state, "annotations"))
})

test("a daemon older than the panel is flagged rather than silently degraded", () => {
  const v1 = Model.applyMessage(Model.emptyState(), { kind: "hello", protocol: 1, version: "0.1.0" })
  assert.ok(Model.needsBackendUpgrade(v1))

  const v2 = Model.applyMessage(Model.emptyState(), { kind: "hello", protocol: 2, version: "0.2.0" })
  assert.ok(!Model.needsBackendUpgrade(v2))

  // Disconnected is not the same as outdated.
  assert.ok(!Model.needsBackendUpgrade(Model.emptyState()))
})

test("pins_changed replaces the pin list wholesale", () => {
  let state = Model.emptyState()
  state = Model.applyMessage(state, {
    kind: "event",
    event: "pins_changed",
    pins: [
      { id: 2, width: 300, height: 420, created_at_unix_ms: 20, thumbnail: "/t/2.png", revision: 1 },
      { id: 1, width: 640, height: 360, created_at_unix_ms: 10, thumbnail: "/t/1.png", revision: 0 }
    ]
  })
  assert.equal(Model.pinCount(state), 2)
  // Sorted by id so the grid does not reshuffle between events.
  assert.deepEqual(state.pins.map((p) => p.id), [1, 2])

  state = Model.applyMessage(state, { kind: "event", event: "pins_changed", pins: [] })
  assert.equal(Model.pinCount(state), 0)
})

test("malformed pins are dropped instead of reaching a delegate", () => {
  const pins = Model.sanitizePins([
    { id: 1, width: 10, height: 10 },
    { id: 0 },                       // ids start at 1
    { id: -3 },
    { id: "abc" },
    null,
    "nope",
    { width: 10 }                    // no id at all
  ])
  assert.equal(pins.length, 1)
  assert.equal(pins[0].id, 1)
  assert.equal(pins[0].thumbnail, "")
  assert.equal(pins[0].revision, 0)
})

test("a v1 daemon's summaries decode with thumbnail and revision defaulted", () => {
  const pins = Model.sanitizePins([{ id: 1, width: 8, height: 8, created_at_unix_ms: 5 }])
  assert.equal(pins[0].thumbnail, "")
  assert.equal(pins[0].revision, 0)
  assert.equal(Model.thumbnailUrl(pins[0]), "")
})

test("applyMessage does not mutate the state it was given", () => {
  // QML only re-evaluates bindings on reassignment, so returning a new
  // object is load-bearing, not a style preference.
  const before = Model.emptyState()
  const after = Model.applyMessage(before, { kind: "hello", protocol: 2, version: "1" })
  assert.equal(before.connected, false)
  assert.notEqual(before, after)
})

test("unknown message kinds leave the state alone", () => {
  const before = Model.applyMessage(Model.emptyState(), { kind: "hello", protocol: 2, version: "0.2.0" })
  const after = Model.applyMessage(before, { kind: "something-new-in-v3", data: {} })
  assert.deepEqual(after, before)
})

test("daemon errors surface a human-readable line", () => {
  const withMessage = Model.applyMessage(Model.emptyState(), {
    kind: "error",
    data: { kind: "capture_failed", message: "slurp not found" }
  })
  assert.equal(withMessage.lastError, "slurp not found")

  const noImage = Model.applyMessage(Model.emptyState(), {
    kind: "error",
    data: { kind: "clipboard_no_image" }
  })
  assert.equal(noImage.lastError, "The clipboard does not contain an image")

  // Cancelling a capture is a normal outcome, not an error to display.
  const canceled = Model.applyMessage(Model.emptyState(), {
    kind: "error",
    data: { kind: "capture_canceled" }
  })
  assert.equal(canceled.lastError, "")
})

test("a successful reply clears a stale error", () => {
  let state = Model.applyMessage(Model.emptyState(), {
    kind: "error",
    data: { kind: "capture_failed", message: "boom" }
  })
  assert.notEqual(state.lastError, "")
  state = Model.applyMessage(state, { kind: "ok" })
  assert.equal(state.lastError, "")
})

// -------------------------------------------------------- presentation

test("thumbnail URL carries the revision so a rotate busts Qt's image cache", () => {
  const pin = { thumbnail: "/run/user/1000/osnip/osnip-thumbs/3.png", revision: 0 }
  const before = Model.thumbnailUrl(pin)
  const after = Model.thumbnailUrl({ ...pin, revision: 1 })
  assert.ok(before.startsWith("file:///run/user/1000/"))
  assert.notEqual(before, after, "same URL after a transform would render stale pixels")
})

test("the bar badge stays narrow", () => {
  assert.equal(Model.countLabel(0), "")
  assert.equal(Model.countLabel(1), "1")
  assert.equal(Model.countLabel(99), "99")
  assert.equal(Model.countLabel(100), "99+")
})

test("tooltip distinguishes not-running from no-pins and pluralizes", () => {
  assert.equal(Model.tooltipText(Model.emptyState()), "Osnip · not running")
  assert.equal(Model.tooltipText({ connected: true, pins: [] }), "Osnip · no pins")
  assert.equal(Model.tooltipText({ connected: true, pins: [{ id: 1 }] }), "Osnip · 1 pin")
  assert.equal(Model.tooltipText({ connected: true, pins: [{ id: 1 }, { id: 2 }] }), "Osnip · 2 pins")
})

test("size and age captions", () => {
  assert.equal(Model.sizeLabel({ width: 640, height: 360 }), "640×360")
  const now = 1_000_000_000
  assert.equal(Model.ageLabel(now, now), "now")
  assert.equal(Model.ageLabel(now, now - 45 * 1000), "now")
  assert.equal(Model.ageLabel(now, now - 4 * 60 * 1000), "4m")
  assert.equal(Model.ageLabel(now, now - 3 * 3600 * 1000), "3h")
  assert.equal(Model.ageLabel(now, now - 50 * 3600 * 1000), "2d")
  // Clock skew must not print a negative age.
  assert.equal(Model.ageLabel(now, now + 60_000), "now")
})

test("grid never asks for more columns than there are pins, or than the schema allows", () => {
  assert.equal(Model.gridColumns(3, 10), 3)
  assert.equal(Model.gridColumns(3, 2), 2)
  assert.equal(Model.gridColumns(3, 0), 3)
  // Out-of-range settings are clamped to the manifest's min/max.
  assert.equal(Model.gridColumns(99, 10), 5)
  assert.equal(Model.gridColumns(0, 10), 3)
  assert.equal(Model.gridColumns("nonsense", 10), 3)
})

test("cursor clamps at the edges instead of wrapping", () => {
  assert.equal(Model.moveCursor(0, -1, 3), 0)
  assert.equal(Model.moveCursor(2, 1, 3), 2)
  assert.equal(Model.moveCursor(0, 1, 3), 1)
  // Vertical movement is a whole row at a time and may overshoot.
  assert.equal(Model.moveCursor(0, 3, 5), 3)
  assert.equal(Model.moveCursor(4, 3, 5), 4)
  assert.equal(Model.moveCursor(0, 1, 0), 0)
})

// ------------------------------------------------------------- install

test("installing runs in a terminal the user can see and never elevates silently", () => {
  const args = Model.installProcessArgs()
  assert.deepEqual(args.slice(0, 6), [
    "omarchy", "launch", "floating", "terminal", "with", "presentation"
  ])
  const command = args[6]
  assert.match(command, /omarchy pkg aur add osnip/)
  // No privilege escalation from inside omarchy-shell.
  assert.doesNotMatch(command, /\bsudo\b|\bpkexec\b|\byay\b/)
})

test("install reports completion and failure, so the panel cannot spin forever", () => {
  const command = Model.installCommand()
  const { complete, failed } = Model.installSentinelPaths()
  assert.ok(command.includes(complete), "no completion sentinel")
  assert.ok(command.includes(failed), "no failure sentinel")

  // The panel has to actually watch for both.
  const qml = read("Panel.qml")
  assert.ok(qml.includes("installSentinelPaths().complete"))
  assert.ok(qml.includes("installSentinelPaths().failed"))
})

test("capture and clipboard go through the CLI, which can cold-start the daemon", () => {
  assert.deepEqual(Model.cliArgs("capture"), ["osnip", "capture"])
  assert.deepEqual(Model.cliArgs("clipboard"), ["osnip", "clipboard"])
  assert.deepEqual(Model.backendProbeArgs(), ["which", "osnip"])
})

// -------------------------------------------------------------- manifest

test("manifest satisfies the shell's plugin schema", () => {
  const manifest = JSON.parse(read("manifest.json"))
  assert.equal(manifest.schemaVersion, 1)
  assert.equal(manifest.id, Model.PLUGIN_ID)
  assert.ok(!manifest.id.startsWith("omarchy."), "omarchy.* is a reserved namespace")
  assert.deepEqual(manifest.kinds, ["bar-widget"])
  // A declared kind without its entry point installs and then does nothing.
  assert.ok(manifest.entryPoints.barWidget)
  assert.ok(["left", "center", "right"].includes(manifest.barWidget.defaultSection))
  for (const key of ["id", "name", "version", "kinds", "entryPoints"]) {
    assert.ok(manifest[key] !== undefined, `missing required field ${key}`)
  }
})

test("every manifest entry point exists on disk", () => {
  const manifest = JSON.parse(read("manifest.json"))
  for (const [kind, file] of Object.entries(manifest.entryPoints)) {
    assert.ok(!file.startsWith("/"), `${kind} entry point must be relative`)
    assert.ok(!file.includes(".."), `${kind} entry point must not escape the plugin dir`)
    assert.ok(fs.existsSync(path.join(__dirname, "..", file)), `${kind} -> ${file} not found`)
  }
})

test("the QML and the manifest agree on the plugin id", () => {
  const manifest = JSON.parse(read("manifest.json"))
  for (const file of ["BarWidget.qml", "Panel.qml"]) {
    assert.ok(read(file).includes(manifest.id), `${file} does not name ${manifest.id}`)
  }
})

test("the bar host's widget contract is fully forwarded", () => {
  // Omitting any of these breaks `omarchy-shell shell toggle` routing or
  // the bar's one-popup-at-a-time coordinator — silently, at runtime.
  const qml = read("BarWidget.qml")
  for (const member of [
    "property bool opened", "function open()", "function close()",
    "property bool popoutSwitchClosing", "function closeForPopoutSwitch()"
  ]) {
    assert.ok(qml.includes(member), `BarWidget.qml is missing ${member}`)
  }
})

test("the socket is recreated per attempt, never reused", () => {
  // Quickshell's Socket cannot be reconnected once it has dropped:
  // assigning `connected = true` to a used instance is silently ignored,
  // with no error and no signal. The daemon is spawned on first capture
  // and restarts across upgrades, so reconnecting is the normal case —
  // a reused Socket means the bar goes stale until the shell restarts.
  const qml = read("Panel.qml")
  assert.match(qml, /socketLoader\.active = false\s*\n\s*socketLoader\.active = true/,
    "connectBackend must remount the Loader to get a fresh Socket")
  assert.doesNotMatch(qml, /\bbackendSocket\b/,
    "a singleton Socket id is the reuse pattern that cannot reconnect")
})

test("the subscribe line is written through the socket instance, not root.send()", () => {
  // onConnectedChanged fires while the Loader is still constructing the
  // Socket, so socketLoader.item is null and root.send() would drop the
  // line. The daemon picks its framing from the client's first byte, so
  // a dropped subscribe means no `hello` ever arrives and the panel
  // waits forever on a socket that is genuinely open.
  const qml = read("Panel.qml")
  const handler = qml
    .slice(qml.indexOf("onConnectedChanged"), qml.indexOf("onError"))
    // Strip comments: the block explains why root.send() is wrong here,
    // and prose must not trip the assertion below.
    .replace(/\/\/.*$/gm, "")
  assert.ok(handler.length > 0, "could not locate onConnectedChanged")
  assert.match(handler, /write\(Model\.subscribeRequest\(\)/,
    "subscribe must be written through the instance")
  assert.doesNotMatch(handler, /root\.send\(/,
    "root.send() reads socketLoader.item, which is null at this point")
})

test("settings declared in the manifest are the ones the QML reads", () => {
  const manifest = JSON.parse(read("manifest.json"))
  const qml = read("BarWidget.qml") + read("Panel.qml")
  for (const entry of manifest.barWidget.schema) {
    assert.ok(
      qml.includes(`setting("${entry.key}"`),
      `manifest offers "${entry.key}" but no QML reads it`
    )
  }
})
