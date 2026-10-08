# Maintainer: Zohaib <zohaibbaig144@gmail.com>
#
# Builds Zohara Terminal directly from this checkout rather than fetching a
# source tarball — there's no tagged release yet, and this is what the
# `.github/workflows/package.yml` CI job runs against a fresh clone to
# produce an installable `.pkg.tar.zst` on every push. To build locally on
# an Arch machine, run `makepkg -si` from the repo root.
#
# Installs four binaries from the workspace (see crates/app/*/Cargo.toml
# for what each one is): `zohara` (the safety-check CLI), `ut-tui` (the
# real interactive multiplexer — this is "the terminal" day to day),
# `ut-gui` (the native windowed front end, reusing ut-tui's own App), and
# `ut-app` (a minimal demo binary, not meant for daily use). `ut-tui` is
# also installed under the friendlier name `zohara-terminal`.
pkgname=zohara-terminal
pkgver=0.1.0
pkgrel=1
pkgdesc="Universal terminal application for Arch-based Linux — real multiplexer, SSH, workflows, plugins"
arch=('x86_64' 'aarch64')
url="https://github.com/Zohaib8090/zohara-terminal"
license=('MIT')
depends=('gcc-libs' 'openssl' 'libxkbcommon' 'libxcb' 'libx11' 'mesa')
makedepends=('rust' 'pkgconf')
source=()
sha256sums=()

build() {
  cd "$startdir"
  cargo build --release --workspace --locked
}

package() {
  cd "$startdir"

  install -Dm755 target/release/ut-tui "$pkgdir/usr/bin/ut-tui"
  ln -s ut-tui "$pkgdir/usr/bin/zohara-terminal"
  install -Dm755 target/release/ut-gui "$pkgdir/usr/bin/ut-gui"
  install -Dm755 target/release/zohara "$pkgdir/usr/bin/zohara"
  install -Dm755 target/release/ut-app "$pkgdir/usr/bin/ut-app"

  install -Dm644 packaging/zohara-terminal.desktop \
    "$pkgdir/usr/share/applications/zohara-terminal.desktop"

  install -Dm644 README.md "$pkgdir/usr/share/doc/$pkgname/README.md"
  install -Dm644 docs/SPEC.md "$pkgdir/usr/share/doc/$pkgname/SPEC.md"
  install -Dm644 LICENSE-MIT "$pkgdir/usr/share/licenses/$pkgname/LICENSE-MIT"
}
