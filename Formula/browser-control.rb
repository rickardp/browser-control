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
  version "0.2.2"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/rickardp/browser-control/releases/download/v0.2.2/browser-control-aarch64-apple-darwin.tar.gz"
      sha256 "c3e961168c3de7db89e2530efdcd113b8d5a2f391836474d5d64c30f296fdcbe"
    end
    on_intel do
      url "https://github.com/rickardp/browser-control/releases/download/v0.2.2/browser-control-x86_64-apple-darwin.tar.gz"
      sha256 "122fba62e9c459e22c081fffb3d392aec2d12e7db04ac356e5553b578f98ea45"
    end
  end

  on_linux do
    on_intel do
      url "https://github.com/rickardp/browser-control/releases/download/v0.2.2/browser-control-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "787210908f91f08d54864f9dc24e8a7b7dd451888c5b3fa75b31ff944afa3bec"
    end
    on_arm do
      url "https://github.com/rickardp/browser-control/releases/download/v0.2.2/browser-control-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "59f5d4dfa60a0da85699ac41a768a293b8bcfec27cdaeb86e5d220cb6d14cc30"
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
