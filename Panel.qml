import QtQuick
import QtQuick.Controls
import Quickshell
import Quickshell.Io
import qs.Commons
import qs.Ui
import "Model.js" as Model

// Osnip's panel: the live pin list, per-pin actions, and the capture
// verbs, backed by a persistent NDJSON connection to osnip-daemon.
//
// The connection is the interesting part. The daemon speaks two
// framings on one socket and picks by first byte, so writing a line
// that opens with `{` selects the newline-delimited one — which stays
// open and pushes a `pins_changed` snapshot on every change. Nothing
// here polls: the bar count updates the instant a pin opens or closes.
//
// All decoding lives in Model.js so it can be unit-tested outside QML.
// This file draws; it does not decide.
Panel {
  id: root
  moduleName: "io.github.franciscogranda.osnip"
  ipcTarget: "io.github.franciscogranda.osnip"

  property var anchorItem: null
  property var hostWidget: null
  readonly property var barIdentity: hostWidget || root

  // ------------------------------------------------------------ state

  property var state: Model.emptyState()
  property bool backendInstalled: false
  property bool backendChecked: false
  property bool installing: false
  property string installError: ""
  property int cursorIndex: 0
  property bool cursorActive: false
  property real nowMs: Date.now()

  readonly property bool backendConnected: state.connected === true
  readonly property var pins: state.pins || []
  readonly property int pinTotal: pins.length
  readonly property string socketPath: Model.socketPath(Quickshell.env("XDG_RUNTIME_DIR"))
  readonly property int columns: Model.gridColumns(root.setting("thumbnailColumns", 3), pinTotal)

  readonly property color foreground: root.barForeground
  readonly property string fontFamily: bar ? bar.fontFamily : Style.font.family

  // A daemon predating protocol v2 has no thumbnails and no per-pin
  // actions. Say so plainly rather than drawing buttons that no-op.
  readonly property bool needsBackendUpgrade: Model.needsBackendUpgrade(state)

  readonly property string statusLine: {
    if (!backendChecked) return "Checking…"
    if (!backendInstalled) return "The osnip daemon is not installed"
    if (!backendConnected) return "Daemon not running"
    if (needsBackendUpgrade) return "osnip " + state.version + " — update for previews"
    return "osnip " + state.version + " · connected"
  }

  // ------------------------------------------------------- connection

  function connectBackend() {
    if (backendSocket.connected || root.socketPath === "") return
    if (!root.backendInstalled) return
    backendSocket.connected = true
  }

  function send(payload) {
    if (!backendSocket.connected) return false
    backendSocket.write(payload + "\n")
    backendSocket.flush()
    return true
  }

  // A line from the daemon. Model.parseLine never throws, and an
  // unrecognized `kind` folds to an unchanged state — a plugin that
  // raises here would take the whole bar down with it.
  function handleLine(raw) {
    var message = Model.parseLine(raw)
    if (!message) return
    var next = Model.applyMessage(root.state, message)
    root.state = next
    if (root.cursorIndex > next.pins.length - 1) {
      root.cursorIndex = Math.max(0, next.pins.length - 1)
    }
  }

  // ---------------------------------------------------------- actions

  // Capture and clipboard go through the CLI, not the socket: the CLI
  // auto-spawns the daemon when none is listening, so these two work
  // from a cold start where a socket write would simply fail.
  function runCli(subcommand) {
    cliProcess.command = Model.cliArgs(subcommand)
    cliProcess.startDetached()
  }

  function captureRegion() {
    // Region selection takes over the screen; leaving the panel open
    // would put it between the user and the region they are dragging.
    root.close()
    runCli("capture")
  }

  function pinClipboard() {
    root.close()
    runCli("clipboard")
  }

  function closeAllPins() {
    if (!send(Model.closeAllRequest())) runCli("close-all")
  }

  function closePin(id) {
    send(Model.closeRequest(id))
  }

  function pinAction(id, action) {
    if (!Model.isPinAction(action)) return
    send(Model.pinActionRequest(id, action))
  }

  // --------------------------------------------------------- keyboard

  function moveCursor(delta) {
    if (root.pinTotal === 0) return
    root.cursorActive = true
    root.cursorIndex = Model.moveCursor(root.cursorIndex, delta, root.pinTotal)
  }

  function currentPin() {
    if (root.cursorIndex < 0 || root.cursorIndex >= root.pinTotal) return null
    return root.pins[root.cursorIndex]
  }

  function activateCursor() {
    var pin = currentPin()
    if (pin) root.pinAction(pin.id, "copy")
  }

  function deleteCursor() {
    var pin = currentPin()
    if (pin) root.closePin(pin.id)
  }

  // ---------------------------------------------------------- install

  function beginInstall() {
    if (root.installing) return
    root.installError = ""
    root.installing = true
    installProcess.command = Model.installProcessArgs()
    installProcess.startDetached()
    installWatchTimer.restart()
  }

  onOpenedChanged: {
    if (!opened) {
      root.cursorActive = false
      return
    }
    root.nowMs = Date.now()
    root.cursorIndex = 0
    // The user is looking at it now, so stop being patient.
    root.reconnectDelay = root.reconnectMin
    probeBackend()
    connectBackend()
  }

  function probeBackend() {
    if (probeProcess.running) return
    probeProcess.command = Model.backendProbeArgs()
    probeProcess.running = true
  }

  Component.onCompleted: probeBackend()

  // ------------------------------------------------------- processes

  Socket {
    id: backendSocket
    path: root.socketPath
    connected: false
    parser: SplitParser {
      splitMarker: "\n"
      onRead: function (line) { root.handleLine(line) }
    }
    onConnectedChanged: {
      if (connected) {
        root.reconnectDelay = root.reconnectMin
        // Subscribing is also what selects the NDJSON framing: the
        // leading `{` is the discriminator the daemon reads.
        root.send(Model.subscribeRequest())
      } else {
        // Nothing imperative here: reconnectTimer's `running` is bound
        // to this socket's state, and calling restart() would replace
        // that binding with a one-shot value — which is exactly how a
        // daemon started after the shell ended up never being noticed.
        root.state = Model.emptyState()
      }
    }
    onError: function (error) {
      backendSocket.connected = false
    }
  }

  // The daemon is not always running — it is spawned on the first
  // capture, which may be hours into a session. Reconnecting is
  // deliberately NOT gated on the panel being open: the bar badge is
  // supposed to be live, so a daemon that starts while the panel is
  // closed still has to reach the icon.
  //
  // Each failed attempt costs a Quickshell socket warning in the
  // journal, so the delay backs off toward RECONNECT_MAX rather than
  // retrying at a fixed cadence for the rest of the session.
  readonly property int reconnectMin: 2000
  readonly property int reconnectMax: 30000
  property int reconnectDelay: reconnectMin

  Timer {
    id: reconnectTimer
    interval: root.reconnectDelay
    repeat: true
    running: root.backendInstalled && !backendSocket.connected
    onTriggered: {
      root.reconnectDelay = Math.min(root.reconnectMax, root.reconnectDelay * 2)
      root.connectBackend()
    }
  }

  // Only for the "3m ago" captions, and only while the panel is open.
  Timer {
    interval: 30000
    repeat: true
    running: root.opened
    onTriggered: root.nowMs = Date.now()
  }

  Process {
    id: probeProcess
    onExited: function (exitCode) {
      root.backendInstalled = exitCode === 0
      root.backendChecked = true
      if (root.backendInstalled) root.connectBackend()
    }
  }

  Process { id: cliProcess }
  Process { id: installProcess }

  // A presented terminal reports no exit status back to us, so the
  // install command drops sentinel files and we watch for them. Without
  // this the panel could not tell a finished install from a terminal
  // the user closed, and would spin forever.
  Timer {
    id: installWatchTimer
    interval: 1500
    repeat: true
    running: root.installing
    onTriggered: {
      installSentinelProcess.command = [
        "bash", "-c",
        'if [[ -f "' + Model.installSentinelPaths().complete + '" ]]; then echo done; '
          + 'elif [[ -f "' + Model.installSentinelPaths().failed + '" ]]; then echo failed; fi'
      ]
      installSentinelProcess.running = true
    }
  }

  Process {
    id: installSentinelProcess
    stdout: SplitParser {
      onRead: function (line) {
        var result = String(line).trim()
        if (result === "done") {
          root.installing = false
          root.installError = ""
          root.probeBackend()
        } else if (result === "failed") {
          root.installing = false
          root.installError = "Installation did not finish. Check the Omarchy terminal and try again."
        }
      }
    }
  }

  // -------------------------------------------------------------- UI

  KeyboardPanel {
    id: panel
    anchorItem: root.anchorItem
    owner: root.barIdentity
    bar: root.bar
    open: root.opened
    centerOnBar: false
    focusTarget: keyCatcher
    contentWidth: panel.fittedContentWidth(Style.space(360))
    contentHeight: panel.fittedContentHeight(contentColumn.implicitHeight, Style.space(560))

    PanelKeyCatcher {
      id: keyCatcher
      anchors.fill: parent
      onMoveRequested: function (dx, dy) {
        if (dy !== 0) root.moveCursor(dy * root.columns)
        else if (dx !== 0) root.moveCursor(dx)
      }
      onActivateRequested: root.activateCursor()
      onDeleteRequested: root.deleteCursor()
      onCloseRequested: root.close()
      onTabRequested: function (direction) { root.switchPanel(direction) }

      ScrollView {
        id: scrollArea
        anchors.fill: parent
        clip: true
        contentWidth: availableWidth
        ScrollBar.horizontal.policy: ScrollBar.AlwaysOff
        ScrollBar.vertical.policy: ScrollBar.AsNeeded

        Column {
          id: contentColumn
          width: scrollArea.availableWidth
          spacing: Style.space(14)

          // ------------------------------------------------- hero
          Item {
            width: parent.width
            implicitHeight: Math.max(heroIcon.implicitHeight, heroLabels.implicitHeight)

            Item {
              id: heroIcon
              anchors.left: parent.left
              anchors.verticalCenter: parent.verticalCenter
              implicitWidth: heroGlyph.implicitWidth
              implicitHeight: heroGlyph.implicitHeight
              opacity: root.backendConnected ? 1.0 : 0.6

              Text {
                id: heroGlyph
                text: "󰹑"
                color: root.foreground
                font.family: root.fontFamily
                font.pixelSize: Style.font.display
              }

              Text {
                visible: root.backendConnected
                anchors.right: heroGlyph.right
                anchors.bottom: heroGlyph.bottom
                anchors.rightMargin: -Style.space(2)
                anchors.bottomMargin: -Style.space(1)
                text: "󰄬"
                color: Color.accent
                font.family: root.fontFamily
                font.pixelSize: Style.font.caption
                font.bold: true
              }
            }

            Column {
              id: heroLabels
              anchors.left: heroIcon.right
              anchors.leftMargin: Style.space(12)
              anchors.right: parent.right
              anchors.verticalCenter: parent.verticalCenter
              spacing: Style.spacing.labelGap

              Text {
                width: parent.width
                text: root.pinTotal === 0
                  ? "No pins"
                  : root.pinTotal + (root.pinTotal === 1 ? " pin" : " pins")
                color: root.foreground
                font.family: root.fontFamily
                font.pixelSize: Style.font.title
                elide: Text.ElideRight
              }

              Text {
                width: parent.width
                text: root.statusLine
                color: Color.muted
                font.family: root.fontFamily
                font.pixelSize: Style.font.caption
                elide: Text.ElideRight
              }
            }
          }

          // ------------------------------------- backend missing
          Column {
            visible: root.backendChecked && !root.backendInstalled
            width: parent.width
            spacing: Style.space(8)

            PanelSeparator { foreground: root.foreground }

            Text {
              width: parent.width
              text: "Osnip needs its daemon — the part that captures regions and "
                + "holds the pin windows on screen. Installing opens an Omarchy "
                + "terminal so you can see what runs."
              color: Color.muted
              font.family: root.fontFamily
              font.pixelSize: Style.font.caption
              wrapMode: Text.WordWrap
            }

            Button {
              text: root.installing ? "Installing…" : "Install the osnip daemon"
              enabled: !root.installing
              onClicked: root.beginInstall()
            }

            Text {
              visible: root.installError !== ""
              width: parent.width
              text: root.installError
              color: Color.urgent
              font.family: root.fontFamily
              font.pixelSize: Style.font.caption
              wrapMode: Text.WordWrap
            }
          }

          // ------------------------------------------ quick actions
          Column {
            visible: root.backendInstalled
            width: parent.width
            spacing: Style.space(8)

            PanelSeparator { foreground: root.foreground }

            Row {
              width: parent.width
              spacing: Style.spacing.controlGap

              Button {
                text: "󰄀  Capture"
                onClicked: root.captureRegion()
              }

              Button {
                text: "󰆒  Clipboard"
                onClicked: root.pinClipboard()
              }

              Button {
                text: "󰗩  Close all"
                enabled: root.pinTotal > 0
                onClicked: root.closeAllPins()
              }
            }

            Text {
              visible: root.state.lastError !== ""
              width: parent.width
              text: root.state.lastError
              color: Color.urgent
              font.family: root.fontFamily
              font.pixelSize: Style.font.caption
              wrapMode: Text.WordWrap
            }
          }

          // -------------------------------------------- pin grid
          Column {
            visible: root.backendInstalled
            width: parent.width
            spacing: Style.space(8)

            PanelSeparator { foreground: root.foreground }

            PanelSectionHeader {
              text: "PINS"
              foreground: root.foreground
              fontFamily: root.fontFamily
            }

            Text {
              visible: root.pinTotal === 0
              width: parent.width
              text: root.backendConnected
                ? "Nothing pinned. Capture a region and it stays on top while you work."
                : "The daemon starts on your first capture."
              color: Color.muted
              font.family: root.fontFamily
              font.pixelSize: Style.font.caption
              wrapMode: Text.WordWrap
            }

            Grid {
              id: pinGrid
              visible: root.pinTotal > 0
              width: parent.width
              columns: root.columns
              spacing: Style.space(8)

              readonly property real cellWidth: columns > 0
                ? (width - spacing * (columns - 1)) / columns
                : width

              Repeater {
                model: root.pins

                delegate: Item {
                  id: cell
                  required property var modelData
                  required property int index

                  // Driven by the shared cursor rather than by this
                  // cell's own hover: the action buttons sit on top of
                  // the hover area, so a `containsMouse` reading would
                  // go false the moment the pointer reached a button
                  // and take the buttons away with it.
                  readonly property bool hot: root.cursorActive && index === root.cursorIndex

                  width: pinGrid.cellWidth
                  implicitHeight: preview.height + caption.implicitHeight + Style.space(4)

                  Rectangle {
                    id: preview
                    width: parent.width
                    // A 4:3 well keeps the grid on a regular baseline
                    // whatever shape the captures are.
                    height: Math.round(parent.width * 0.75)
                    radius: Style.cornerRadius
                    color: Util.alpha(root.foreground, 0.06)
                    border.width: cell.hot ? Math.max(1, Style.space(1)) : 0
                    border.color: Color.accent

                    Image {
                      anchors.fill: parent
                      anchors.margins: Style.space(3)
                      source: Model.thumbnailUrl(cell.modelData)
                      fillMode: Image.PreserveAspectFit
                      asynchronous: true
                      // The daemon rewrites a thumbnail in place after a
                      // rotate or flip; without this Qt would serve the
                      // pre-transform pixels from its own cache.
                      cache: false
                      visible: status === Image.Ready
                    }

                    // Shown when the daemon is too old to write
                    // thumbnails, or while one is still decoding.
                    Text {
                      anchors.centerIn: parent
                      visible: !cell.modelData.thumbnail
                      text: "󰋩"
                      color: Color.muted
                      font.family: root.fontFamily
                      font.pixelSize: Style.font.heading
                    }

                    // Per-pin actions, revealed on hover or when the
                    // keyboard cursor lands here. Five buttons under
                    // every cell all the time would drown the previews.
                    Rectangle {
                      anchors.left: parent.left
                      anchors.right: parent.right
                      anchors.bottom: parent.bottom
                      height: actionRow.implicitHeight + Style.space(6)
                      visible: cell.hot && !root.needsBackendUpgrade
                      radius: Style.cornerRadius
                      color: Util.alpha(Color.popups.background, 0.85)

                      Row {
                        id: actionRow
                        anchors.centerIn: parent
                        spacing: Style.space(2)

                        PanelActionButton {
                          iconText: "󰅖"
                          tooltipText: "Close pin"
                          foreground: root.foreground
                          fontFamily: root.fontFamily
                          fontSize: Style.font.iconSmall
                          onClicked: root.closePin(cell.modelData.id)
                        }
                        PanelActionButton {
                          iconText: "󰆏"
                          tooltipText: "Copy to clipboard"
                          foreground: root.foreground
                          fontFamily: root.fontFamily
                          fontSize: Style.font.iconSmall
                          onClicked: root.pinAction(cell.modelData.id, "copy")
                        }
                        PanelActionButton {
                          iconText: "󰆓"
                          tooltipText: "Save as PNG"
                          foreground: root.foreground
                          fontFamily: root.fontFamily
                          fontSize: Style.font.iconSmall
                          onClicked: root.pinAction(cell.modelData.id, "save")
                        }
                        PanelActionButton {
                          iconText: "󰑧"
                          tooltipText: "Rotate 90°"
                          foreground: root.foreground
                          fontFamily: root.fontFamily
                          fontSize: Style.font.iconSmall
                          onClicked: root.pinAction(cell.modelData.id, "rotate_right")
                        }
                        PanelActionButton {
                          iconText: "󰹳"
                          tooltipText: "Flip horizontally"
                          foreground: root.foreground
                          fontFamily: root.fontFamily
                          fontSize: Style.font.iconSmall
                          onClicked: root.pinAction(cell.modelData.id, "flip_h")
                        }
                      }
                    }

                    // Moves the cursor here on hover; sits under the
                    // action row so the buttons keep their clicks.
                    MouseArea {
                      anchors.fill: parent
                      hoverEnabled: true
                      z: -1
                      onEntered: {
                        root.cursorActive = true
                        root.cursorIndex = cell.index
                      }
                    }
                  }

                  Text {
                    id: caption
                    anchors.top: preview.bottom
                    anchors.topMargin: Style.space(4)
                    width: parent.width
                    text: Model.sizeLabel(cell.modelData)
                      + " · " + Model.ageLabel(root.nowMs, cell.modelData.createdAt)
                    color: cell.hot ? root.foreground : Color.muted
                    font.family: root.fontFamily
                    font.pixelSize: Style.font.caption
                    horizontalAlignment: Text.AlignHCenter
                    elide: Text.ElideRight
                  }
                }
              }
            }

            Text {
              visible: root.needsBackendUpgrade
              width: parent.width
              text: "The osnip daemon is older than this panel: no previews and "
                + "no per-pin actions. Update with: omarchy pkg aur add osnip"
              color: Color.muted
              font.family: root.fontFamily
              font.pixelSize: Style.font.caption
              wrapMode: Text.WordWrap
            }
          }
        }
      }
    }
  }
}
