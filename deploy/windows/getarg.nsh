; GetArg: read `/NAME=value` from the command line. The value runs to the next " /" (or the
; end), so it may contain "/" (URLs, paths) -- unlike FileFunc's GetOptions, which cuts
; at the first "/". Surrounding double quotes are removed.
;   !insertmacro GetArg "$CMDLINE-params" "/SERVER=" $Server

!macro GetArg PARAMS NAME OUT
  Push `${PARAMS}`
  Push `${NAME}`
  Call GetArgFn
  Pop ${OUT}
!macroend

Function GetArgFn
  Exch $1 ; name
  Exch
  Exch $0 ; params
  Push $2
  Push $3
  Push $4
  Push $5
  StrLen $5 $1
  StrCpy $2 0
  StrCpy $4 ""
  getarg_find:
    StrCpy $3 $0 $5 $2
    StrCmp $3 "" getarg_done
    StrCmp $3 $1 getarg_found
    IntOp $2 $2 + 1
    Goto getarg_find
  getarg_found:
    IntOp $2 $2 + $5
    StrCpy $0 $0 "" $2
    StrCpy $2 0
  getarg_scan:
    StrCpy $3 $0 2 $2
    StrCmp $3 "" getarg_cut
    StrCmp $3 " /" getarg_cut
    IntOp $2 $2 + 1
    Goto getarg_scan
  getarg_cut:
    StrCpy $4 $0 $2
  getarg_trim:
    StrCpy $3 $4 1 -1
    StrCmp $3 " " 0 getarg_unquote
    StrCpy $4 $4 -1
    Goto getarg_trim
  getarg_unquote:
    StrCpy $3 $4 1
    StrCmp $3 '"' 0 getarg_done
    StrCpy $4 $4 "" 1
    StrCpy $3 $4 1 -1
    StrCmp $3 '"' 0 getarg_done
    StrCpy $4 $4 -1
  getarg_done:
  Pop $5
  Exch $4 ; result <-> saved $4
  Exch
  Pop $3
  Exch
  Pop $2
  Exch
  Pop $0
  Exch
  Pop $1
FunctionEnd
