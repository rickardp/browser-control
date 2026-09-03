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
  version "1.2.0"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/rickardp/browser-control/releases/download/v1.2.0/browser-control-aarch64-apple-darwin.tar.gz"
      sha256 "d50052bf4f6d056f030756e6e1cbbb0fb79d625b9d12913628f2d5e807e2955e"
    end
    on_intel do
      url "https://github.com/rickardp/browser-control/releases/download/v1.2.0/browser-control-x86_64-apple-darwin.tar.gz"
      sha256 "09b789e78161d2f1b4547061dd16124d1463b4782ab1fee4422624ca997e17a3"
    end
  end

  on_linux do
    on_intel do
      url "https://github.com/rickardp/browser-control/releases/download/v1.2.0/browser-control-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "066e727737c52a8e84fee24504f9d6ae9580953078fd49fd0eb81e5cbd673518"
    end
    on_arm do
      url "https://github.com/rickardp/browser-control/releases/download/v1.2.0/browser-control-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "9583afb4795b2387123f9da9c87830a632cd85cb355701b48efeff4a8c0edb67"
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
