; Allow Veyndesk through Windows Firewall (direct connections, LAN server) on any network profile.
!macro NSIS_HOOK_POSTINSTALL
  nsExec::Exec 'netsh advfirewall firewall delete rule name="VeloDesk"'
  nsExec::Exec 'netsh advfirewall firewall delete rule name="Veyndesk"'
  nsExec::Exec 'netsh advfirewall firewall add rule name="Veyndesk" dir=in action=allow program="$INSTDIR\Veyndesk.exe" enable=yes profile=any'
!macroend

!macro NSIS_HOOK_POSTUNINSTALL
  nsExec::Exec 'netsh advfirewall firewall delete rule name="Veyndesk"'
!macroend
