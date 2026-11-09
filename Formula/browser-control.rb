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
  version "1.4.0"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/rickardp/browser-control/releases/download/v1.4.0/browser-control-aarch64-apple-darwin.tar.gz"
      sha256 "ff45e2899d8c22980277c9560c35e9139c2f8783f89c3462829e3ba3d6ee6f25"
    end
    on_intel do
      url "https://github.com/rickardp/browser-control/releases/download/v1.4.0/browser-control-x86_64-apple-darwin.tar.gz"
      sha256 "f86acb6a9f9374a54627baa081b45aa5ace1319a74d5f243a6416664f9b82e49"
    end
  end

  on_linux do
    on_intel do
      url "https://github.com/rickardp/browser-control/releases/download/v1.4.0/browser-control-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "6fe543e0c055cb70b7ebf2b8df63ac2c122c276d6248219a5fe766e32e12b346"
    end
    on_arm do
      url "https://github.com/rickardp/browser-control/releases/download/v1.4.0/browser-control-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "aa57e3db4089a97c2f37d09d6b22d9b4eb7a6e1a9d27dc4f7bdf30adb780fa5d"
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
