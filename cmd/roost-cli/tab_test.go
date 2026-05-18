package main

import (
	"strings"
	"testing"

	"github.com/charliek/roost/internal/ipc"
	"github.com/spf13/cobra"
)

func TestPrintTabTree(t *testing.T) {
	tree := ipc.TabListResult{
		Projects: []ipc.TabListProject{
			{
				ID:   1,
				Name: "alpha",
				Tabs: []ipc.TabListTab{
					{ID: 10, Title: "active-tab", AgentState: "running", IsActive: true},
					{ID: 11, Title: "alert-tab", HasNotification: true},
				},
			},
			{
				ID:   2,
				Name: "beta",
				Tabs: []ipc.TabListTab{
					{ID: 20, Title: "plain-tab"},
				},
			},
		},
	}

	out := captureStdout(t, func() { printTabTree(tree) })

	cases := []struct {
		needle string
		why    string
	}{
		{"alpha (id=1)", "project header includes id"},
		{"* [10] active-tab", "active marker is a leading asterisk"},
		{"  [11] alert-tab  state=- [!]", "notification marker is [!] and missing state shows as '-'"},
		{"state=running", "agent state surfaces inline"},
		{"beta (id=2)", "second project rendered"},
		{"  [20] plain-tab", "non-active prefix is two spaces"},
	}
	for _, c := range cases {
		if !strings.Contains(out, c.needle) {
			t.Errorf("missing %q (%s)\nfull output:\n%s", c.needle, c.why, out)
		}
	}
}

func TestCompleteTabIDsHasNoListenerIsSilent(t *testing.T) {
	resetFlagsForTest(t)
	t.Setenv("ROOST_SOCKET", "/tmp/no-such-socket-roost-test")
	clientCtx.SocketPath = "/tmp/no-such-socket-roost-test"

	suggestions, directive := completeTabIDs(&cobra.Command{}, nil, "")
	if len(suggestions) != 0 {
		t.Errorf("expected no suggestions when GUI unreachable; got %v", suggestions)
	}
	if directive != cobra.ShellCompDirectiveNoFileComp {
		t.Errorf("expected NoFileComp directive; got %v", directive)
	}
}
