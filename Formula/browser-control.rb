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
  version "0.3.1"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/rickardp/browser-control/releases/download/v0.3.1/browser-control-aarch64-apple-darwin.tar.gz"
      sha256 "f66f53d4a58f4b727eb529f862b75a0064f2a96fc8d264019e417536ae9e8731"
    end
    on_intel do
      url "https://github.com/rickardp/browser-control/releases/download/v0.3.1/browser-control-x86_64-apple-darwin.tar.gz"
      sha256 "3c0e250f949591c82c8ea6c814b3fce27dffda34f3de202b7b3ad0f3a9b53d0e"
    end
  end

  on_linux do
    on_intel do
      url "https://github.com/rickardp/browser-control/releases/download/v0.3.1/browser-control-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "f62185a7ee8c0c4d54ed9205ff6b18c0f834b2ff3972efd778784f073a5c4824"
    end
    on_arm do
      url "https://github.com/rickardp/browser-control/releases/download/v0.3.1/browser-control-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "bce905341e4b40e18f6155d8de373900e96a42d1b07ff1c7367b78676bdd6e12"
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
