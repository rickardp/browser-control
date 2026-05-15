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
  version "0.3.0"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/rickardp/browser-control/releases/download/v0.3.0/browser-control-aarch64-apple-darwin.tar.gz"
      sha256 "08c0104a49495f4d87101997a1d28352a17d156487669f63033961c45fa486ea"
    end
    on_intel do
      url "https://github.com/rickardp/browser-control/releases/download/v0.3.0/browser-control-x86_64-apple-darwin.tar.gz"
      sha256 "a82227a2aca990d2d9883b524c9980d8399861d8232bc0df317b78ae50d52026"
    end
  end

  on_linux do
    on_intel do
      url "https://github.com/rickardp/browser-control/releases/download/v0.3.0/browser-control-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "51eca1e356968ec6ffde7d37f79b697318c1970a7762585400c308934523a4da"
    end
    on_arm do
      url "https://github.com/rickardp/browser-control/releases/download/v0.3.0/browser-control-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "a6e63ddc533aef98b27fce2ceaa0cabce92dcaae16bd7e700a1623672694f92c"
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
