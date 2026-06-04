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
  version "1.0.0"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/rickardp/browser-control/releases/download/v1.0.0/browser-control-aarch64-apple-darwin.tar.gz"
      sha256 "a10007a4695e68e4f308f2e94747a22a56bd9b63653e21091b30e42f4d175056"
    end
    on_intel do
      url "https://github.com/rickardp/browser-control/releases/download/v1.0.0/browser-control-x86_64-apple-darwin.tar.gz"
      sha256 "ead36ed7498bce6440302c6a32e1587c76eeb2b83e7b06193a8a00e051f85371"
    end
  end

  on_linux do
    on_intel do
      url "https://github.com/rickardp/browser-control/releases/download/v1.0.0/browser-control-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "8988a9474da96938f56274df23e3b1e98819295bcc359621908e31631b44f950"
    end
    on_arm do
      url "https://github.com/rickardp/browser-control/releases/download/v1.0.0/browser-control-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "09a92a86145221386326ba38300c917824f4f4fc11fd17e5d1e38d3372aef26a"
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
