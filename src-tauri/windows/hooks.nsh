; All app data lives in data\ under the install dir (see data_root in src/config.rs), which the stock uninstaller
; leaves alone; delete it when the delete-app-data box is ticked ($DeleteAppDataCheckboxState) and this is not an update.
; Require the .embymini marker so installing into a folder that already had a data\ never deletes someone else's files.
!macro NSIS_HOOK_POSTUNINSTALL
  ${If} $DeleteAppDataCheckboxState = 1
  ${AndIf} $UpdateMode <> 1
  ${AndIf} ${FileExists} "$INSTDIR\data\.embymini"
    RMDir /r "$INSTDIR\data"
    RMDir "$INSTDIR"
  ${EndIf}
!macroend
