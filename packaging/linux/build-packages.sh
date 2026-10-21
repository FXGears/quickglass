#!/usr/bin/env bash
# Builds the Linux .deb and .rpm from target/release/quickglass.
#
# Layout (both packages):
#   /usr/bin/quickglass
#   /usr/share/applications/quickglass.desktop   MimeType= -> "Open with" for .md / text
#   /usr/share/icons/hicolor/{64x64,512x512,scalable}/apps/quickglass.*
#   /usr/share/doc/quickglass/                   README, CHANGELOG, LICENSE
#
# Runtime dependencies are derived from the binary's ELF NEEDED entries, not
# hand-written: dpkg-shlibdeps for the .deb, rpmbuild's automatic Requires for
# the .rpm. Desktop database and icon cache refresh run through the distros'
# own file triggers (desktop-file-utils, hicolor/gtk icon cache), so the
# packages carry no maintainer scripts.
#
# Needs: dpkg-dev, rpm (rpmbuild). Run build-release.sh first.
# Output: dist/quickglass_<ver>_amd64.deb, dist/quickglass-<ver>-1.x86_64.rpm
set -euo pipefail
cd "$(dirname "$0")/../.."

VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
BIN=target/release/quickglass
DIST=dist
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
mkdir -p "$DIST"

# glibc floor. Rust's std references a few newer glibc symbols (pidfd_spawnp,
# 2.39) as *weak* and falls back at runtime when they are absent; glibc's loader
# treats a missing weak version as non-fatal (elf/dl-version.c, match_symbol).
# dpkg-shlibdeps and rpmbuild count those anyway, which would wrongly block
# install on Ubuntu 22.04/24.04, Debian 12, RHEL 9. The real floor is the
# highest GLIBC version the binary needs non-weakly (Flags: none).
version_needs() {  # $1 = none|WEAK
    readelf -V "$BIN" | sed -n '/\.gnu\.version_r/,$p' \
        | awk -v f="$1" '$3 ~ /^GLIBC_/ && $5 == f {sub("GLIBC_","",$3); print $3}' | sort -V
}
GLIBC_FLOOR=$(version_needs none | tail -1)
# Weak-only versions above the floor: excluded from the generated RPM Requires.
WEAK_ONLY=$({ echo "$GLIBC_FLOOR"; version_needs WEAK; } | sort -Vu | sed "1,/^${GLIBC_FLOOR//./\\.}\$/d" | tr '\n' ' ')
echo "glibc floor: $GLIBC_FLOOR (weak-only above floor: ${WEAK_ONLY:-none})"

# Shared file tree, staged once.
stage() {
    local root=$1
    install -Dm755 "$BIN" "$root/usr/bin/quickglass"
    install -Dm644 packaging/linux/quickglass.desktop "$root/usr/share/applications/quickglass.desktop"
    install -Dm644 resources/icon-64.png "$root/usr/share/icons/hicolor/64x64/apps/quickglass.png"
    install -Dm644 resources/icon-512.png "$root/usr/share/icons/hicolor/512x512/apps/quickglass.png"
    install -Dm644 resources/icon.svg "$root/usr/share/icons/hicolor/scalable/apps/quickglass.svg"
    install -Dm644 README.md "$root/usr/share/doc/quickglass/README.md"
    install -Dm644 CHANGELOG.md "$root/usr/share/doc/quickglass/CHANGELOG.md"
    install -Dm644 LICENSE "$root/usr/share/doc/quickglass/copyright"
}

# ---------------------------------------------------------------- .deb
DEB="$WORK/deb"
stage "$DEB"
# dpkg-shlibdeps needs a debian/control in the working directory.
mkdir -p "$WORK/shlib/debian"
printf 'Source: quickglass\n\nPackage: quickglass\nArchitecture: amd64\n' > "$WORK/shlib/debian/control"
DEPENDS=$(cd "$WORK/shlib" && dpkg-shlibdeps -O "$DEB/usr/bin/quickglass" 2>/dev/null \
    | sed -n 's/^shlibs:Depends=//p' \
    | sed -E "s/libc6 \(>= [0-9.]+\)/libc6 (>= $GLIBC_FLOOR)/")
SIZE=$(du -sk "$DEB/usr" | cut -f1)
mkdir -p "$DEB/DEBIAN"
cat > "$DEB/DEBIAN/control" <<EOF
Package: quickglass
Version: $VERSION
Architecture: amd64
Maintainer: FXGears <noreply@github.com>
Installed-Size: $SIZE
Depends: $DEPENDS
Section: text
Priority: optional
Homepage: https://github.com/FXGears/quickglass
Description: Fast, lightweight markdown viewer
 Opens a markdown file in a native X11 window in about 30 ms.
 Search, text selection, links and anchors. No browser engine.
EOF
dpkg-deb --root-owner-group --build "$DEB" "$DIST/quickglass_${VERSION}_amd64.deb" >/dev/null

# ---------------------------------------------------------------- .rpm
RPM="$WORK/rpm"
mkdir -p "$RPM"/{BUILD,RPMS,SPECS,SOURCES,SRPMS}
cat > "$RPM/SPECS/quickglass.spec" <<EOF
Name:           quickglass
Version:        $VERSION
Release:        1
Summary:        Fast, lightweight markdown viewer
License:        GPL-3.0-only
URL:            https://github.com/FXGears/quickglass
BuildArch:      x86_64
# Requires are generated automatically from the binary's linked libraries;
# weak-only glibc versions (see GLIBC_FLOOR in build-packages.sh) are dropped.
$( [ -n "$WEAK_ONLY" ] && printf '%%global __requires_exclude ^libc\\\\.so\\\\.6\\\\(GLIBC_(%s)\\\\)\n' "$(echo $WEAK_ONLY | sed 's/\./\\\\./g; s/ /|/g')" )

%description
Opens a markdown file in a native X11 window in about 30 ms.
Search, text selection, links and anchors. No browser engine.

%install
cp -a "$WORK/rpmroot/." %{buildroot}/

%files
%{_bindir}/quickglass
%{_datadir}/applications/quickglass.desktop
%{_datadir}/icons/hicolor/64x64/apps/quickglass.png
%{_datadir}/icons/hicolor/512x512/apps/quickglass.png
%{_datadir}/icons/hicolor/scalable/apps/quickglass.svg
%doc %{_docdir}/quickglass/README.md
%doc %{_docdir}/quickglass/CHANGELOG.md
%license %{_docdir}/quickglass/copyright
EOF
stage "$WORK/rpmroot"
rpmbuild --quiet --dbpath "$WORK/rpmdb" -bb --define "_topdir $RPM" "$RPM/SPECS/quickglass.spec"
cp "$RPM"/RPMS/x86_64/quickglass-"$VERSION"-1.x86_64.rpm "$DIST/"

ls -l "$DIST"/quickglass_"$VERSION"_amd64.deb "$DIST"/quickglass-"$VERSION"-1.x86_64.rpm
