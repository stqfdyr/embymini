; 程序把所有数据放在安装目录下的 data\（见 src/config.rs 的 data_root）。
; 内置卸载流程只删自己装的文件，所以勾了「删除应用程序数据」、且不是自动更新时，在这里删 data\。
; 只认带 .embymini 标记的目录：装在一个本来就有 data 文件夹的目录里时，不误删别人的东西。
!macro NSIS_HOOK_POSTUNINSTALL
  ${If} $DeleteAppDataCheckboxState = 1
  ${AndIf} $UpdateMode <> 1
  ${AndIf} ${FileExists} "$INSTDIR\data\.embymini"
    RMDir /r "$INSTDIR\data"
    RMDir "$INSTDIR"
  ${EndIf}
!macroend
