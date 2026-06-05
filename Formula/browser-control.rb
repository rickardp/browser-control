# Homebrew formula stub for `browser-control`.
#
# This file is a TEMPLATE. The release workflow (`.github/workflows/release.yml`,
# `homebrew-bump` job) renders it with concrete values via `sed` and commits
# the result to `Formula/browser-control.rb` in this repo (Pattern A:
# in-repo tap). Users install with:
#
#   brew tap rickardp/browser-control https://github.com/rickardp/browser-control.git
#   brew install browser-control
#
# Placeholders rendered by the workflow (each surrounded by the literal
# at-sign markers shown below; this comment uses different delimiters so
# `sed` doesn't rewrite the documentation block):
#   {VERSION}           the released version (no leading `v`)
#   {URL_DARWIN_ARM}    full GH Releases URL for aarch64-apple-darwin tarball
#   {SHA_DARWIN_ARM}    sha256 of that tarball
#   {URL_DARWIN_X86}    full GH Releases URL for x86_64-apple-darwin tarball
#   {SHA_DARWIN_X86}    sha256 of that tarball
#   {URL_LINUX_X86}     full GH Releases URL for x86_64-unknown-linux-gnu tarball
#   {SHA_LINUX_X86}     sha256 of that tarball
#   {URL_LINUX_ARM}     full GH Releases URL for aarch64-unknown-linux-gnu tarball
#   {SHA_LINUX_ARM}     sha256 of that tarball

class BrowserControl < Formula
  desc "CLI for browser lifecycle and CDP/BiDi access for agent-driven dev"
  homepage "https://github.com/rickardp/browser-control"
  version "1.0.1"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/rickardp/browser-control/releases/download/v1.0.1/browser-control-aarch64-apple-darwin.tar.gz"
      sha256 "9062f3664f0627b1001a8ae9c56957a456862230cca645f5621a32f2a48ff915"
    end
    on_intel do
      url "https://github.com/rickardp/browser-control/releases/download/v1.0.1/browser-control-x86_64-apple-darwin.tar.gz"
      sha256 "f48e9e6c1686a5b15ecf96adf29a40039df557370279663f5a44fa913808193c"
    end
  end

  on_linux do
    on_intel do
      url "https://github.com/rickardp/browser-control/releases/download/v1.0.1/browser-control-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "b5439934cb2e17cf272831a4f15a7a22ab26a2763d507107c7419d50fc65af86"
    end
    on_arm do
      url "https://github.com/rickardp/browser-control/releases/download/v1.0.1/browser-control-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "8301be643fcdbddfe2e4c80482982ea257c38ce6e5fa75705a6b9fe3806200f6"
    end
  end

  def install
    bin.install "browser-control"
  end

  test do
    assert_match "browser-control", shell_output("#{bin}/browser-control --version")
    # list-installed must run without a registry; exit code is best-effort
    # (CI runners typically have no browsers installed, which is allowed).
    system "#{bin}/browser-control", "list-installed"
  end
end
