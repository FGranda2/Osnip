# Maintainer: Francisco Granda <pancho.fg23@hotmail.com>
pkgname=osnip
# GitHub names a tag tarball after the *repository*, which is capitalised,
# while an AUR pkgname must be lowercase — so the extracted directory is
# not "$pkgname-$pkgver" and every build step has to cd here instead.
_repo=Osnip
pkgver=0.2.1
pkgrel=1
pkgdesc="Snipaste-style screen pinning for wlroots-style Wayland compositors (Niri, Hyprland/Omarchy)"
arch=('x86_64' 'aarch64')
url="https://github.com/FGranda2/Osnip"
license=('MIT')
# Linked directly (ldd): expat, libdrm, mesa. Resolved at runtime via
# dlopen, so invisible to ldd but still required: libwayland-client
# (wayland), libxkbcommon, libEGL (mesa). slurp and wl-clipboard are
# executed, not linked.
depends=('wayland' 'libxkbcommon' 'mesa' 'libdrm' 'expat' 'slurp' 'wl-clipboard')
# Copy and save work without it; they just produce no desktop toast.
optdepends=('libnotify: desktop notifications when copying or saving a pin')
makedepends=('rust' 'cargo' 'pkgconf')
source=("$pkgname-$pkgver.tar.gz::$url/archive/refs/tags/v$pkgver.tar.gz")
sha256sums=('f99a68ab9dd001277829fef882ff79724fab9cb46746ce3a0430f1d271e0f544')

prepare() {
  cd "$_repo-$pkgver"
  # Vendor up front so build() can run offline, as the Arch packaging
  # guidelines require.
  export RUSTUP_TOOLCHAIN=stable
  cargo fetch --locked --target "$(rustc -vV | sed -n 's/host: //p')"
}

build() {
  cd "$_repo-$pkgver"
  export RUSTUP_TOOLCHAIN=stable
  export CARGO_TARGET_DIR=target
  cargo build --frozen --release --workspace
}

check() {
  cd "$_repo-$pkgver"
  export RUSTUP_TOOLCHAIN=stable
  # Capture and clipboard need a live Wayland session and are exercised
  # manually; everything else runs headless.
  cargo test --frozen --release --workspace
}

package() {
  cd "$_repo-$pkgver"
  install -Dm755 "target/release/osnip"        "$pkgdir/usr/bin/osnip"
  install -Dm755 "target/release/osnip-daemon" "$pkgdir/usr/bin/osnip-daemon"

  # A *user* unit: the daemon owns Wayland windows and must run inside
  # the graphical session, never as a system service. It is optional —
  # the CLI auto-spawns the daemon on first use — so it ships disabled.
  #
  # The in-tree unit points at ~/.local/bin, which is right for the
  # build-from-source path the README documents but wrong here: this
  # copy sits in /usr/lib/systemd/user alongside a binary in /usr/bin,
  # and would fail to start for anyone who never built from source.
  install -Dm644 "contrib/osnip-daemon.service" \
    "$pkgdir/usr/lib/systemd/user/osnip-daemon.service"
  sed -i 's|^ExecStart=.*$|ExecStart=/usr/bin/osnip-daemon|' \
    "$pkgdir/usr/lib/systemd/user/osnip-daemon.service"

  install -Dm644 LICENSE   "$pkgdir/usr/share/licenses/$pkgname/LICENSE"
  install -Dm644 README.md "$pkgdir/usr/share/doc/$pkgname/README.md"

  # Compositor integration snippets, referenced from the README and by
  # the Osnip bar plugin's setup instructions.
  install -Dm644 "contrib/omarchy/osnip.lua" \
    "$pkgdir/usr/share/doc/$pkgname/contrib/omarchy/osnip.lua"
  install -Dm644 "contrib/niri/config-snippet.kdl" \
    "$pkgdir/usr/share/doc/$pkgname/contrib/niri/config-snippet.kdl"
}
