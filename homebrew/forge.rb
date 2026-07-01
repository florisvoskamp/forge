# Homebrew formula for Forge.
#
#   brew tap florisvoskamp/forge https://github.com/florisvoskamp/forge
#   brew install forge
#
# version + sha256 below are filled in per release (the checksums.txt asset
# produced by .github/workflows/release.yml has the values). Until then, the
# `curl | sh` installer (install.sh) is the recommended path.
class Forge < Formula
  desc "Multi-provider mesh AI coding CLI"
  homepage "https://github.com/florisvoskamp/forge"
  version "2.2.0" # auto-updated by release.yml (scripts/update-brew-formula.sh) per release
  license "AGPL-3.0-only"

  on_macos do
    on_arm do
      url "https://github.com/florisvoskamp/forge/releases/download/v#{version}/forge-aarch64-apple-darwin.tar.gz"
      sha256 "e31fb58f14132b22d2987fc9830907d1f4347525ff0a37be021f60dece46560d"
    end
    on_intel do
      url "https://github.com/florisvoskamp/forge/releases/download/v#{version}/forge-x86_64-apple-darwin.tar.gz"
      sha256 "8b4eb0f43a85de7ae7acbd4e0d9de7f511c23604449d4bf5e92c0df5f973ab9e"
    end
  end

  on_linux do
    on_intel do
      url "https://github.com/florisvoskamp/forge/releases/download/v#{version}/forge-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "b3bb62596b3c5f5e922877b2e2546fe79e0accac55b17dc7ac6c2a5080bdf3cb"
    end
    on_arm do
      url "https://github.com/florisvoskamp/forge/releases/download/v#{version}/forge-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "cb28558116028c6cf101656622ed7e16489dc15ab6a9b49f3a90e9d46a785e5e"
    end
  end

  def install
    bin.install "forge"
    # Completions + man page are bundled in releases that built them (xtasks gen-dist, wired into
    # release.yml). Guard so the formula still installs from older asset sets without them.
    if File.exist?("completions/forge.bash")
      bash_completion.install "completions/forge.bash" => "forge"
      zsh_completion.install "completions/_forge"
      fish_completion.install "completions/forge.fish"
    end
    man1.install "forge.1" if File.exist?("forge.1")
  end

  test do
    assert_match "forge", shell_output("#{bin}/forge --version")
  end
end
