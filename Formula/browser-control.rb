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
  version "0.3.5"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/rickardp/browser-control/releases/download/v0.3.5/browser-control-aarch64-apple-darwin.tar.gz"
      sha256 "e92d0987690a0e85b9ace5252c8c2c7b742a9dc49cd6525d8cec731ab1f99560"
    end
    on_intel do
      url "https://github.com/rickardp/browser-control/releases/download/v0.3.5/browser-control-x86_64-apple-darwin.tar.gz"
      sha256 "9d5d6a473dd49bc8fdd2894f034ac6f17dd1cf0a302f183304942fa5acb23272"
    end
  end

  on_linux do
    on_intel do
      url "https://github.com/rickardp/browser-control/releases/download/v0.3.5/browser-control-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "ec84b6c0051137694c8de31eec2f0f3681cda42fdb47da0f3e1daa954be4b47b"
    end
    on_arm do
      url "https://github.com/rickardp/browser-control/releases/download/v0.3.5/browser-control-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "f5f523acf9b77b53f081e513485dced570cf1467d4229265d86c91f08c8f6987"
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
