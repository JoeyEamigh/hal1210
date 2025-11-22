install-systemd:
  @echo "installing systemd service file"
  mkdir -p ~/.config/systemd/user/
  sed "s|<<PWD>>|$(pwd)|g" hal1210.service.in > ~/.config/systemd/user/hal1210.service
  systemctl --user daemon-reload
