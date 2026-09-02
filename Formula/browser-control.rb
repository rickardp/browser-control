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
  version "1.1.1"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/rickardp/browser-control/releases/download/v1.1.1/browser-control-aarch64-apple-darwin.tar.gz"
      sha256 "389716360ec06485904ac48b70a66bb33bc5ccd354b08dcab5f90fd37591d6fe"
    end
    on_intel do
      url "https://github.com/rickardp/browser-control/releases/download/v1.1.1/browser-control-x86_64-apple-darwin.tar.gz"
      sha256 "36151115562b901a480f8ce99b05d5226b67b0a7893bcd896ed6d881ba09e9fe"
    end
  end

  on_linux do
    on_intel do
      url "https://github.com/rickardp/browser-control/releases/download/v1.1.1/browser-control-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "a6003f255785b069cc3f6f80a7c7e824d59101c11b1e2c5904a1a6232e7d5a76"
    end
    on_arm do
      url "https://github.com/rickardp/browser-control/releases/download/v1.1.1/browser-control-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "6f2c427af957857148e156bec5fe0bfb3bb42775f75154cb0be4ab65689c9b0a"
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
