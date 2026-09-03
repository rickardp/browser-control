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
  version "1.2.1"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/rickardp/browser-control/releases/download/v1.2.1/browser-control-aarch64-apple-darwin.tar.gz"
      sha256 "0fed3ce4ba64763d6b76280658fe4a2046a4bf42f923ab1578fd04be6f6313c2"
    end
    on_intel do
      url "https://github.com/rickardp/browser-control/releases/download/v1.2.1/browser-control-x86_64-apple-darwin.tar.gz"
      sha256 "96fb79a0e9fc56af831df05a9519180e02613aebfbaafa5d8f22d37a9d209773"
    end
  end

  on_linux do
    on_intel do
      url "https://github.com/rickardp/browser-control/releases/download/v1.2.1/browser-control-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "6c122aba6987ee90202d4f68299bca343085405a459a47ba6069486fafd020c4"
    end
    on_arm do
      url "https://github.com/rickardp/browser-control/releases/download/v1.2.1/browser-control-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "da8c3afa7fe677e224e0578ecf8568c738d8cb98782c3b1a401391bfb750ba2f"
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
