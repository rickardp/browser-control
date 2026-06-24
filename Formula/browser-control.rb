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
  version "1.0.2"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/rickardp/browser-control/releases/download/v1.0.2/browser-control-aarch64-apple-darwin.tar.gz"
      sha256 "c2c4be099fe0f96e9057ff2d5b3280071343b6240f993ad0e3b0109d5e160203"
    end
    on_intel do
      url "https://github.com/rickardp/browser-control/releases/download/v1.0.2/browser-control-x86_64-apple-darwin.tar.gz"
      sha256 "b739314f4bc059796b678a1eb15f754437be5573ec6fe712d1aa88bde0c2c2bf"
    end
  end

  on_linux do
    on_intel do
      url "https://github.com/rickardp/browser-control/releases/download/v1.0.2/browser-control-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "0f4bbbbe68b2add520fa445d27c7900d6ed8fc036f1bad48975f523e40c0b365"
    end
    on_arm do
      url "https://github.com/rickardp/browser-control/releases/download/v1.0.2/browser-control-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "9b0d45adb806dc39bb7380e2f68684e01ad94815a47adc0c4d594b84f7504e0c"
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
