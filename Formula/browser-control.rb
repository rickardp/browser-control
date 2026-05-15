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
  version "0.3.3"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/rickardp/browser-control/releases/download/v0.3.3/browser-control-aarch64-apple-darwin.tar.gz"
      sha256 "11d0e9e0a185a566242e1c27292e6372d7ad864c882729eb20a3fab66dec0c43"
    end
    on_intel do
      url "https://github.com/rickardp/browser-control/releases/download/v0.3.3/browser-control-x86_64-apple-darwin.tar.gz"
      sha256 "b8dd4403aa0abf8b09bdf051d0d91e22c2f2bf6ed2accf7e1c70adec9355da9d"
    end
  end

  on_linux do
    on_intel do
      url "https://github.com/rickardp/browser-control/releases/download/v0.3.3/browser-control-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "f0055fbe9176e4c1a1562fd49161227cf28c40e0fae45eed7aaba373a9e74fab"
    end
    on_arm do
      url "https://github.com/rickardp/browser-control/releases/download/v0.3.3/browser-control-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "38fbeb4f8fc266e49764298eed7cccf632c53b2ab47c537e0eaafb0bef2ece41"
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
