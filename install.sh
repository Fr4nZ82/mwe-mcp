#!/bin/sh
# mwe-mcp installer — download-and-run (roadmap 5m).
#
#   curl -fsSL https://raw.githubusercontent.com/Fr4nZ82/mwe-mcp/main/install.sh | sh
#
# Downloads the prebuilt `mwe-mcp` binary for this OS/arch from the GitHub
# Releases, verifies its SHA-256, and installs it. The binary bundles the
# Candle embedder, so it needs no external embedding service.
#
# Env knobs:
#   MWE_MCP_VERSION   tag to install (default: latest release)
#   MWE_MCP_BINDIR    install dir    (default: $HOME/.local/bin)
#
# Windows: download the `…-x86_64-pc-windows-msvc.zip` from the Releases page
# (this POSIX script targets Linux and macOS).

set -eu

REPO="Fr4nZ82/mwe-mcp"
BINDIR="${MWE_MCP_BINDIR:-$HOME/.local/bin}"

say() { printf '%s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || die "missing required command: $1"; }

need curl
need tar

os="$(uname -s)"
arch="$(uname -m)"
case "$os" in
  Linux)
    case "$arch" in
      x86_64 | amd64) target="x86_64-unknown-linux-gnu" ;;
      *) die "unsupported Linux arch: $arch (only x86_64 is published today)" ;;
    esac ;;
  Darwin)
    case "$arch" in
      arm64 | aarch64) target="aarch64-apple-darwin" ;;
      x86_64) target="x86_64-apple-darwin" ;;
      *) die "unsupported macOS arch: $arch" ;;
    esac ;;
  *)
    die "unsupported OS: $os — on Windows download the .zip from https://github.com/$REPO/releases" ;;
esac

version="${MWE_MCP_VERSION:-latest}"
if [ "$version" = "latest" ]; then
  say "Resolving latest release…"
  version="$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" \
    | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -n1)"
  [ -n "$version" ] || die "could not resolve the latest release tag"
fi

asset="mwe-mcp-${version}-${target}.tar.gz"
sha="${asset%.tar.gz}.sha256"   # published as mwe-mcp-<ver>-<target>.sha256 (no .tar.gz)
base="https://github.com/$REPO/releases/download/${version}"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

say "Downloading ${asset} (${version})…"
curl -fSL "${base}/${asset}" -o "$tmp/$asset" || die "download failed: ${base}/${asset}"

# Verify the per-asset SHA-256 if the runner published one.
if curl -fsSL "${base}/${sha}" -o "$tmp/$sha" 2>/dev/null; then
  say "Verifying checksum…"
  if command -v sha256sum >/dev/null 2>&1; then
    expected="$(awk '{print $1}' "$tmp/$sha")"
    actual="$(sha256sum "$tmp/$asset" | awk '{print $1}')"
  else
    expected="$(awk '{print $1}' "$tmp/$sha")"
    actual="$(shasum -a 256 "$tmp/$asset" | awk '{print $1}')"
  fi
  [ "$expected" = "$actual" ] || die "checksum mismatch (expected $expected, got $actual)"
else
  say "warning: no published checksum for ${asset} — skipping verification"
fi

say "Extracting…"
tar -C "$tmp" -xzf "$tmp/$asset"
bin="$(find "$tmp" -type f -name mwe-mcp | head -n1)"
[ -n "$bin" ] || die "could not find the mwe-mcp binary inside ${asset}"

mkdir -p "$BINDIR"
install -m 0755 "$bin" "$BINDIR/mwe-mcp"
say ""
say "Installed mwe-mcp ${version} → $BINDIR/mwe-mcp"

case ":$PATH:" in
  *":$BINDIR:"*) ;;
  *) say "note: $BINDIR is not on your PATH — add it, e.g.:"
     say "      echo 'export PATH=\"$BINDIR:\$PATH\"' >> ~/.profile" ;;
esac

say ""
say "Next:  mwe-mcp serve --workdir ./work"
say "Then open the dashboard URL it prints and finish setup in the browser."
say "(First run downloads the bge-m3 embedding model ~2.2 GB, once.)"
