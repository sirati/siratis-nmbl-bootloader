{
  pkgs,
  openssh,
  fullSystem,
}:

pkgs.writeText "sshd_config" ''
  Port ${toString fullSystem.sshdPort}
  ListenAddress 0.0.0.0
  ListenAddress ::
  PermitRootLogin prohibit-password
  AuthenticationMethods publickey
  PubkeyAuthentication yes
  PasswordAuthentication no
  KbdInteractiveAuthentication no
  PermitEmptyPasswords no
  HostbasedAuthentication no
  IgnoreRhosts yes
  UsePAM no
  StrictModes yes
  AllowUsers root
  DisableForwarding yes
  AllowAgentForwarding no
  AllowTcpForwarding no
  AllowStreamLocalForwarding no
  GatewayPorts no
  PermitTunnel no
  PermitUserEnvironment no
  PermitUserRC no
  X11Forwarding no
  UseDNS no
  MaxAuthTries 3
  MaxSessions 2
  LoginGraceTime 30
  ClientAliveInterval 60
  ClientAliveCountMax 3
  PerSourcePenalties no
  HostKey /etc/ssh/ssh_host_ed25519_key
  AuthorizedKeysFile /root/.ssh/authorized_keys
  SetEnv PATH=/bin:/sbin:/usr/bin:/usr/sbin NIX_PATH=nixpkgs=flake:nixpkgs NMBL_TUI_SOCK=/nmbl-root/nmbl-run/tui.sock
  Subsystem sftp ${openssh}/libexec/sftp-server
  LogLevel VERBOSE
''
