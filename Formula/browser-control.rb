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
  version "0.3.2"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/rickardp/browser-control/releases/download/v0.3.2/browser-control-aarch64-apple-darwin.tar.gz"
      sha256 "34a92e330243b916006608b51a1876c20d002ab51c5b529af65ea0bc2b70c8ad"
    end
    on_intel do
      url "https://github.com/rickardp/browser-control/releases/download/v0.3.2/browser-control-x86_64-apple-darwin.tar.gz"
      sha256 "3fd25cf5a6125d7c6d421577c71ba88a944fcd985758371be5a8952000c29002"
    end
  end

  on_linux do
    on_intel do
      url "https://github.com/rickardp/browser-control/releases/download/v0.3.2/browser-control-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "bd77e8ea45e25bad149d98d53746e1b0330f18ab932f11bb86ca1bfa02d132d4"
    end
    on_arm do
      url "https://github.com/rickardp/browser-control/releases/download/v0.3.2/browser-control-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "3720b77bc10bd534639fe063fe2251deecea7415c7619fb0742060d7ffda28b3"
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
