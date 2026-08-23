import QtQuick
import Quickshell
import qs.Commons
import qs.Ui
import "Model.js" as Model

// Osnip's bar entry point.
//
// The widget itself is deliberately thin: it owns the icon, the pin
// count badge, and the mouse bindings, and delegates all state to
// Panel.qml through a Loader. That split is the shape every first-party
// popup widget uses (see plugins/panels/weather/BarWidget.qml), and the
// forwarded properties below are a contract with the bar host, not
// decoration — see the note above `opened`.
BarWidget {
  id: root
  moduleName: "io.github.fgranda2.osnip"

  // The Loader creates the panel before the host has finished injecting
  // our own properties, so the wiring is re-run whenever either side
  // changes rather than only once at load.
  function injectPanel() {
    var target = panelLoader.item
    if (!target) return
    if ("bar" in target) target.bar = root.bar
    if ("settings" in target) target.settings = root.settings
    if ("anchorItem" in target) target.anchorItem = button
    if ("hostWidget" in target) target.hostWidget = root
  }

  function togglePanel() {
    if (panelLoader.item && panelLoader.item.toggle) panelLoader.item.toggle()
  }

  readonly property var state: panelLoader.item ? panelLoader.item.state : Model.emptyState()
  readonly property bool backendConnected: panelLoader.item ? panelLoader.item.backendConnected === true : false
  readonly property int pinCount: Model.pinCount(state)
  readonly property bool showBadge: String(root.setting("showCountBadge", "On")) !== "Off"

  // Nothing pinned and nothing to manage — recede rather than sit at
  // full contrast next to widgets that are actually reporting something.
  readonly property bool barIconDimmed: !backendConnected || pinCount === 0

  // Shape contract for the bar host. `open`/`close`/`opened` are what
  // Bar.findPanelWidget routes `omarchy-shell shell toggle` through, and
  // `popoutSwitchClosing`/`closeForPopoutSwitch` are what the bar's
  // one-popup-at-a-time coordinator expects. Dropping any of them
  // breaks popup switching silently, so they are forwarded even though
  // this widget never calls them itself.
  readonly property bool opened: panelLoader.item ? panelLoader.item.opened === true : false

  function open() {
    if (panelLoader.item && panelLoader.item.open) panelLoader.item.open()
  }

  function close() {
    if (panelLoader.item && panelLoader.item.close) panelLoader.item.close()
  }

  readonly property bool popoutSwitchClosing: panelLoader.item ? panelLoader.item.popoutSwitchClosing === true : false

  function closeForPopoutSwitch() {
    if (panelLoader.item) panelLoader.item.closeForPopoutSwitch()
  }

  implicitWidth: button.implicitWidth
  implicitHeight: button.implicitHeight

  onBarChanged: injectPanel()
  onSettingsChanged: injectPanel()

  Loader {
    id: panelLoader
    active: true
    source: Qt.resolvedUrl("Panel.qml")
    onLoaded: {
      root.injectPanel()
      // The host may still be assigning `bar`/`settings` this tick;
      // re-inject once the event loop settles so the panel never comes
      // up bound to nulls.
      Qt.callLater(root.injectPanel)
    }
  }

  BarIconButton {
    id: button
    anchors.fill: parent
    bar: root.bar
    text: "󰹑"
    dimmed: root.barIconDimmed
    tooltipText: Model.tooltipText(root.state)

    iconComponent: Component {
      Item {
        OpticalGlyph {
          id: barGlyph
          anchors.fill: parent
          text: button.text
          color: button.foreground
          fontFamily: button.fontFamily
          fontSize: button.fontSize
        }

        // Live pin count, tucked into the glyph's bottom-right corner.
        // Accent-colored because it is the one piece of state on the
        // bar that the user is actually tracking.
        Text {
          visible: root.showBadge && root.pinCount > 0
          anchors.right: barGlyph.right
          anchors.bottom: barGlyph.bottom
          anchors.rightMargin: -Style.space(2)
          anchors.bottomMargin: -Style.space(1)
          text: Model.countLabel(root.pinCount)
          color: Color.accent
          font.family: button.fontFamily
          font.pixelSize: Math.max(7, Math.round(button.fontSize * 0.5))
          font.bold: true
        }
      }
    }

    // Left opens the panel; right and middle are the two captures the
    // user repeats most, so they skip the panel entirely.
    onPressed: function (mouseButton) {
      if (mouseButton === Qt.LeftButton) root.togglePanel()
      else if (mouseButton === Qt.RightButton && panelLoader.item) panelLoader.item.captureRegion()
      else if (mouseButton === Qt.MiddleButton && panelLoader.item) panelLoader.item.pinClipboard()
    }
  }
}
