# Command duration + exit status after every command, and a readable "command not found".
zmodload zsh/datetime 2>/dev/null
autoload -Uz add-zsh-hook

# Block markers for juan-terminal's renderer. They're OSC 8 hyperlinks with a jt:// URI: the
# terminal keeps them on the grid cells, so they scroll and reflow with the text.
#   jt://prompt?cwd=…&node=…&branch=…&dirty=…   row drawn as chips (live) or block header
#   jt://cmd                                      the typed command (bold)
#   jt://end?code=…&ms=…                          end of a block (exit status, duration)
_jt_enc() {
  local s=$1
  s=${s//\%/%25}; s=${s//&/%26}; s=${s//;/%3B}; s=${s// /%20}; s=${s//$'\e'/}
  print -rn -- $s
}

_jt_node_version() {
  # Only inside a JS project; cached per node binary (nvm switches change it).
  local d=$PWD
  while [[ $d != / && $d != $HOME ]]; do
    [[ -f $d/package.json ]] && break
    d=${d:h}
  done
  [[ -f $d/package.json ]] || return
  local bin=${commands[node]}
  [[ -z $bin ]] && return
  if [[ $bin != $_jt_node_bin ]]; then
    _jt_node_bin=$bin
    _jt_node_ver=$($bin -v 2>/dev/null)
  fi
  print -rn -- $_jt_node_ver
}

_jt_git_info() {
  # One git call: branch + number of changed files.
  local line branch="" dirty=0
  while IFS= read -r line; do
    case $line in
      "# branch.head "*) branch=${line#\# branch.head } ;;
      "#"*) ;;
      *) (( dirty++ )) ;;
    esac
  done < <(git --no-optional-locks status --porcelain=v2 --branch 2>/dev/null)
  [[ -n $branch ]] && print -rn -- "$branch $dirty"
}

_jt_preexec() {
  _jt_start=$EPOCHREALTIME
  print -n '\e]8;;\e\\'   # close the jt://cmd link before the command's output
}

_jt_precmd() {
  local code=$?
  print -n '\e]8;;\e\\'
  if [[ -n $_jt_start ]]; then
    local -i ms=$(( (EPOCHREALTIME - _jt_start) * 1000 ))
    unset _jt_start
    # End marker: one linked space (the renderer draws the block edge there).
    printf '\e]8;;jt://end?code=%d&ms=%d\e\\ \e]8;;\e\\\n' $code $ms
  elif [[ -n $_jt_prompted ]]; then
    # Empty Enter / ^C at the prompt: erase the previous (unused) prompt instead of
    # stacking empty blocks.
    print -n '\e[2A\e[J'
  fi
  _jt_prompted=1
  if [[ ${JT_NATIVE_PROMPT:-1} == 1 ]]; then
    local git=$(_jt_git_info) node=$(_jt_node_version) cwd=${(%):-%~}
    local uri="jt://prompt?cwd=$(_jt_enc $cwd)&node=$(_jt_enc $node)&branch=$(_jt_enc ${git% *})&dirty=${git##* }"
    [[ -z $git ]] && uri="jt://prompt?cwd=$(_jt_enc $cwd)&node=$(_jt_enc $node)"
    uri=${uri//\%/%%}
    cwd=${cwd//\%/%%}
    # Line 1: chips (text is a plain fallback); line 2: input, linked as jt://cmd; beam cursor.
    PROMPT=$'%{\e]8;;'"$uri"$'\e\\%}'"$cwd"$'%{\e]8;;\e\\%}\n%{\e[5 q\e]8;;jt://cmd\e\\%}'
    RPROMPT=''
  fi
}

add-zsh-hook preexec _jt_preexec
# Run last so the native prompt wins over themes that set PROMPT in precmd.
_jt_install_precmd_last() {
  precmd_functions=(${precmd_functions:#_jt_precmd} _jt_precmd)
}
add-zsh-hook precmd _jt_precmd
# User .zshrc runs after this file: re-order once on the first prompt.
_jt_first_prompt() {
  _jt_install_precmd_last
  add-zsh-hook -d precmd _jt_first_prompt
  # The first prompt has no previous one to erase.
  unset _jt_prompted
}
precmd_functions=(_jt_first_prompt $precmd_functions)

# Only define it if the user's config didn't (keeps e.g. Homebrew's handler).
if (( ! $+functions[command_not_found_handler] )); then
  command_not_found_handler() {
    local hint
    # Closest known command (cheap: same first letter, edit distance via zsh's spelling list).
    # Closest command (same first letter, Damerau distance; common tools win ties).
    hint=$({ whence -m "${1[1]}*"; print -l ${(k)aliases[(I)${1[1]}*]} } 2>/dev/null | awk -F/ '{print $NF}' | awk -v w="$1" '
      BEGIN{n=split("ls git cd cat cp mv rm mkdir grep find npm npx node pnpm yarn bun cargo python3 pip make docker claude code open less vim",c," ");for(i=1;i<=n;i++)pri[c[i]]=n-i+1}
      function dist(a,b,  i,j,la,lb,d,x){la=length(a);lb=length(b);for(i=0;i<=la;i++)d[i,0]=i;for(j=0;j<=lb;j++)d[0,j]=j;
        for(i=1;i<=la;i++)for(j=1;j<=lb;j++){x=(substr(a,i,1)==substr(b,j,1))?0:1;d[i,j]=d[i-1,j]+1;if(d[i,j-1]+1<d[i,j])d[i,j]=d[i,j-1]+1;if(d[i-1,j-1]+x<d[i,j])d[i,j]=d[i-1,j-1]+x;
          if(i>1&&j>1&&substr(a,i,1)==substr(b,j-1,1)&&substr(a,i-1,1)==substr(b,j,1)&&d[i-2,j-2]+1<d[i,j])d[i,j]=d[i-2,j-2]+1}
        return d[la,lb]}
      $0!=w{k=dist(w,$0); if(best==""||k<bk||(k==bk&&(pri[$0]>pri[best]||(pri[$0]==pri[best]&&length($0)<length(best))))){best=$0;bk=k}}
      END{if(best!=""&&bk<=(length(w)<=3?1:2))print best}')
    print -P "%F{203}✗%f %B${1//\%/%%}%b%F{242}: command not found%f" >&2
    [[ -n $hint ]] && print -P "%F{242}  did you mean %f%F{75}${hint}%f%F{242}?%f" >&2
    return 127
  }
fi

# Claude Code: report Stop/Notification hook events back to juan-terminal ("Claude finished",
# "Claude needs you"). Only for interactive sessions, not for subcommands like `claude mcp`.
if [[ -n $JT_CLAUDE_SETTINGS && -f $JT_CLAUDE_SETTINGS ]] && (( ! $+aliases[claude] )) && (( ! $+functions[claude] )); then
  claude() {
    case $1 in
      mcp|config|update|doctor|install|setup-token|migrate-installer|plugin|plugins|agents|auth|login|logout|-v|--version|-h|--help)
        command claude "$@" ;;
      *)
        if [[ -n $JT_MCP_CONFIG && -f $JT_MCP_CONFIG ]]; then
          # --mcp-config is variadic: keep --settings after it so it doesn't swallow the prompt.
          command claude --mcp-config "$JT_MCP_CONFIG" --settings "$JT_CLAUDE_SETTINGS" "$@"
        else
          command claude --settings "$JT_CLAUDE_SETTINGS" "$@"
        fi ;;
    esac
  }
fi
