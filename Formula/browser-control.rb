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
  version "0.3.4"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/rickardp/browser-control/releases/download/v0.3.4/browser-control-aarch64-apple-darwin.tar.gz"
      sha256 "d525818c794d63e857df9f4e8d9c245f679bc5512dfe5d1caf359bac259ee397"
    end
    on_intel do
      url "https://github.com/rickardp/browser-control/releases/download/v0.3.4/browser-control-x86_64-apple-darwin.tar.gz"
      sha256 "2f274cf67a2f4fd1a403d5a393c64e3e3e7dff4707b6f8796cfba82f8bd05cf7"
    end
  end

  on_linux do
    on_intel do
      url "https://github.com/rickardp/browser-control/releases/download/v0.3.4/browser-control-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "9ef4814d987ac52cd09bfae26df47ee31105118c390a38f3e715e2fffafacef6"
    end
    on_arm do
      url "https://github.com/rickardp/browser-control/releases/download/v0.3.4/browser-control-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "45a0044c2d3cda0c935df763ca36844e4b00bd35d1b686a8f67141e69c9313f0"
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
