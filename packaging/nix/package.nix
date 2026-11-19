# The PC app (`prostir`), for Nix and NixOS: `nix build`, `nix run`, or the flake's
# package in a NixOS or Home Manager configuration.
{
  lib,
  stdenv,
  rustPlatform,
  pkg-config,
  makeWrapper,
  gst_all_1,
  libgbm,
  pipewire,
  pulseaudio,
  kdePackages,
  android-tools,
  libnotify,
  curl,
  xdg-user-dirs,
  grim,
  zenity,
  intel-media-driver,
  mesa,
}:

let
  root = ../..;
  version = (lib.importTOML (root + "/Cargo.toml")).workspace.package.version;
  # GStreamer plugins Prostir builds its pipelines from: va (Intel and AMD encoding,
  # in -bad), nvcodec (NVIDIA, in -bad), x264 (CPU, in -ugly), pipewiresrc, pulse.
  plugins = with gst_all_1; [
    gstreamer
    gst-plugins-base
    gst-plugins-good
    gst-plugins-bad
    gst-plugins-ugly
    pipewire
  ];
  # Programs it runs; the system's own come first.
  tools = [
    pipewire
    pulseaudio
    kdePackages.libkscreen
    kdePackages.qttools
    android-tools
    libnotify
    curl
    xdg-user-dirs
    grim
    zenity
  ];
  # VA-API drivers for encoding on the graphics card. On NixOS the system's own (in
  # /run/opengl-driver) match the hardware; elsewhere Nix's libva cannot load the
  # distribution's drivers, so Intel's and Mesa's (AMD) come along. NVIDIA's encoder
  # comes from the system driver (NixOS: hardware.nvidia).
  vaDrivers = [
    "/run/opengl-driver/lib/dri"
  ]
  # Intel graphics only come with x86 machines.
  ++ lib.optional stdenv.hostPlatform.isx86_64 "${intel-media-driver}/lib/dri"
  ++ [ "${mesa}/lib/dri" ];
in
rustPlatform.buildRustPackage {
  pname = "prostir";
  inherit version;

  src = lib.fileset.toSource {
    inherit root;
    fileset = lib.fileset.unions [
      (root + "/Cargo.toml")
      (root + "/Cargo.lock")
      (root + "/.cargo")
      (root + "/proto")
      (root + "/host")
      (root + "/client")
      # A workspace member (the fake headset for tests): cargo needs it present.
      (root + "/testkit")
      (root + "/packaging/prostir.desktop")
      (root + "/packaging/prostir.svg")
    ];
  };

  cargoLock.lockFile = root + "/Cargo.lock";
  cargoBuildFlags = [
    "-p"
    "prostir-host"
    "--bin"
    "prostir"
  ];
  # The tests need a desktop session and the headset's toolchain; CI runs them.
  doCheck = false;

  nativeBuildInputs = [
    pkg-config
    makeWrapper
  ];
  buildInputs = [
    gst_all_1.gstreamer
    gst_all_1.gst-plugins-base
    gst_all_1.gst-plugins-bad
    libgbm
  ];

  postInstall = ''
    install -Dm644 packaging/prostir.desktop $out/share/applications/prostir.desktop
    install -Dm644 packaging/prostir.svg $out/share/icons/hicolor/scalable/apps/prostir.svg
    wrapProgram $out/bin/prostir \
      --prefix GST_PLUGIN_SYSTEM_PATH_1_0 : "${lib.makeSearchPathOutput "lib" "lib/gstreamer-1.0" plugins}" \
      --set-default LIBVA_DRIVERS_PATH "${lib.concatStringsSep ":" vaDrivers}" \
      --suffix PATH : "${lib.makeBinPath tools}" \
      --set PROSTIR_EXE "$out/bin/prostir"
  '';

  meta = {
    description = "Extra monitors for a Linux PC (KDE Plasma 6 or Hyprland) in a Meta Quest 3";
    homepage = "https://prostir.ishawyha.dev";
    license = lib.licenses.gpl3Plus;
    mainProgram = "prostir";
    platforms = lib.platforms.linux;
  };
}
