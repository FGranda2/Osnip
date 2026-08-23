# Maintainer: Francisco Granda <pancho.fg23@hotmail.com>
pkgname=osnip
pkgver=0.2.0
pkgrel=1
pkgdesc="Snipaste-style screen pinning for wlroots-style Wayland compositors (Niri, Hyprland/Omarchy)"
arch=('x86_64' 'aarch64')
url="https://github.com/franciscogranda/Osnip"
license=('MIT' 'Apache-2.0')
depends=('slurp' 'wl-clipboard' 'libnotify' 'wayland' 'libxkbcommon' 'vulkan-icd-loader')
makedepends=('rust' 'cargo' 'pkgconf')
source=("$pkgname-$pkgver.tar.gz::$url/archive/refs/tags/v$pkgver.tar.gz")
sha256sums=('SKIP')

prepare() {
  cd "$pkgname-$pkgver"
  # Vendor up front so build() can run offline, as the Arch packaging
  # guidelines require.
  export RUSTUP_TOOLCHAIN=stable
  cargo fetch --locked --target "$(rustc -vV | sed -n 's/host: //p')"
}

build() {
  cd "$pkgname-$pkgver"
  export RUSTUP_TOOLCHAIN=stable
  export CARGO_TARGET_DIR=target
  cargo build --frozen --release --workspace
}

check() {
  cd "$pkgname-$pkgver"
  export RUSTUP_TOOLCHAIN=stable
  # Capture and clipboard need a live Wayland session and are exercised
  # manually; everything else runs headless.
  cargo test --frozen --release --workspace
}

package() {
  cd "$pkgname-$pkgver"
  install -Dm755 "target/release/osnip"        "$pkgdir/usr/bin/osnip"
  install -Dm755 "target/release/osnip-daemon" "$pkgdir/usr/bin/osnip-daemon"

  # A *user* unit: the daemon owns Wayland windows and must run inside
  # the graphical session, never as a system service. It is optional —
  # the CLI auto-spawns the daemon on first use — so it ships disabled.
  install -Dm644 "contrib/osnip-daemon.service" \
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
