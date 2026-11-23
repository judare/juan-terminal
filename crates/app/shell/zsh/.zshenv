# juan-terminal shell integration (loaded via ZDOTDIR; your own files are untouched).
# Point ZDOTDIR back to the user's so zsh reads their .zprofile/.zshrc/.zlogin next.
_jt_integration_dir=$ZDOTDIR
if [[ -n ${JT_USER_ZDOTDIR+x} ]]; then
  ZDOTDIR=$JT_USER_ZDOTDIR
else
  unset ZDOTDIR
fi
unset JT_USER_ZDOTDIR
[[ -f ${ZDOTDIR:-$HOME}/.zshenv ]] && source ${ZDOTDIR:-$HOME}/.zshenv
if [[ -o interactive ]]; then
  source "$_jt_integration_dir/jt-integration.zsh"
fi
unset _jt_integration_dir
