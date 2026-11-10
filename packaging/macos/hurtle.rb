cask "hurtle" do
  arch arm: "aarch64", intel: "x86_64"

  version "@VERSION@"
  sha256 arm:   "@SHA_MACOS_AARCH64@",
         intel: "@SHA_MACOS_X86_64@"

  url "https://github.com/arafays/hurtle/releases/download/v#{version}/hurtle-#{version}-macos-#{arch}.dmg"
  name "Hurtle"
  desc "Multi-connection download manager with browser integration"
  homepage "https://github.com/arafays/hurtle"

  livecheck do
    url :url
    strategy :github_latest
  end

  depends_on macos: ">= :big_sur"

  app "Hurtle.app"
  binary "#{appdir}/Hurtle.app/Contents/MacOS/hurtle-cli"

  # The app is only ad-hoc signed, not notarized: drop the quarantine flag.
  postflight do
    system_command "/usr/bin/xattr",
                   args: ["-dr", "com.apple.quarantine", "#{appdir}/Hurtle.app"]
    system_command "#{appdir}/Hurtle.app/Contents/MacOS/hurtle-host",
                   args: ["--register"]
  end

  uninstall_preflight do
    system_command "#{appdir}/Hurtle.app/Contents/MacOS/hurtle-host",
                   args: ["--unregister"]
  end

  uninstall quit: "io.github.arafays.hurtle"

  zap trash: [
    "~/Library/Application Support/hurtle",
  ]

  caveats <<~EOS
    Hurtle is ad-hoc signed but not notarized by Apple. This cask removes the
    quarantine attribute on install; if macOS still refuses to open it, run:
      xattr -dr com.apple.quarantine /Applications/Hurtle.app
    Install the browser extension from the release page:
      https://github.com/arafays/hurtle/releases/tag/v#{version}
  EOS
end
