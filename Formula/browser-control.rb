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
  version "1.1.0"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/rickardp/browser-control/releases/download/v1.1.0/browser-control-aarch64-apple-darwin.tar.gz"
      sha256 "454e1533c369528337cb0c5c6c7bf7afac8864afbd06d037fd4f8dd17fc349d0"
    end
    on_intel do
      url "https://github.com/rickardp/browser-control/releases/download/v1.1.0/browser-control-x86_64-apple-darwin.tar.gz"
      sha256 "cd921bebc6c9ed9b15239d6df42dda00c9fe206bb975f8d1ca62e70963a74a9c"
    end
  end

  on_linux do
    on_intel do
      url "https://github.com/rickardp/browser-control/releases/download/v1.1.0/browser-control-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "dac3fa72e645d18b100dc5a4e28f6205ab87ced2017b603fff997d4a4f52fc7f"
    end
    on_arm do
      url "https://github.com/rickardp/browser-control/releases/download/v1.1.0/browser-control-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "37f6e3c48e19a6bc06fa3c1b881a972fdb348cf728fa3ca6a3997b0c294246d1"
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
