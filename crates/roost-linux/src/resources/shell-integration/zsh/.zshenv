# Roost zsh auto-bootstrap.
#
# Sourced automatically by zsh because Roost sets ZDOTDIR to this
# directory. It restores the user's real ZDOTDIR FIRST (so all their
# real startup files — .zprofile/.zshrc/.zlogin — are found in their own
# dir), sources their real .zshenv, then loads Roost's integration on the
# first prompt (after .zshrc, so the user's config wins). Do NOT source
# this manually — it's the ZDOTDIR shim, not the integration itself
# (that's roost.zsh).
#
# Known limitation: a system /etc/zshenv that hard-sets ZDOTDIR runs
# before this file and defeats the injection; such users source roost.zsh
# manually instead.
#
# KEEP IN SYNC with mac/Sources/Roost/Resources/shell-integration/zsh/.zshenv

if [[ -n "${ROOST_ZSH_ZDOTDIR+X}" ]]; then
    export ZDOTDIR="$ROOST_ZSH_ZDOTDIR"
    unset ROOST_ZSH_ZDOTDIR
else
    unset ZDOTDIR
fi

# try-always so the integration still loads (and the exit code is right)
# even if the user's .zshenv errors.
{
    _roost_uenv="${ZDOTDIR-$HOME}/.zshenv"
    [[ -r "$_roost_uenv" ]] && source "$_roost_uenv"
} always {
    if [[ -o interactive && -n "${ROOST_RESOURCES_DIR:-}" ]]; then
        # Defer to the first precmd so the user's .zshrc finishes first
        # (a .zshrc that clobbers precmd_functions then can't drop us).
        autoload -Uz add-zsh-hook
        _roost_zdotdir_load() {
            add-zsh-hook -d precmd _roost_zdotdir_load
            [[ -r "$ROOST_RESOURCES_DIR/shell-integration/roost.zsh" ]] &&
                source "$ROOST_RESOURCES_DIR/shell-integration/roost.zsh"
            unfunction _roost_zdotdir_load 2>/dev/null
        }
        add-zsh-hook precmd _roost_zdotdir_load
    fi
    unset _roost_uenv
}
