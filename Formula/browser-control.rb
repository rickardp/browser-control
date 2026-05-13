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
  version "0.2.1"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/rickardp/browser-control/releases/download/v0.2.1/browser-control-aarch64-apple-darwin.tar.gz"
      sha256 "59b1fd51130cf5603ea405a4e01cc774bf92f4eba3b90f61739e3f9122b489b8"
    end
    on_intel do
      url "https://github.com/rickardp/browser-control/releases/download/v0.2.1/browser-control-x86_64-apple-darwin.tar.gz"
      sha256 "0576109beb231356317c47d852d5adb203ce6fe4c325ea6ec37d4deb0b07cc7b"
    end
  end

  on_linux do
    on_intel do
      url "https://github.com/rickardp/browser-control/releases/download/v0.2.1/browser-control-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "c5f59a2129b6c6a807e7f85f1c8e727a411977c0dbcb8439af69a03524c8120d"
    end
    on_arm do
      url "https://github.com/rickardp/browser-control/releases/download/v0.2.1/browser-control-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "a64a433123703c560a48fb594d6ab73508dce166b7929ccd508398dc738485e0"
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
