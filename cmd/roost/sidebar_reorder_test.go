package main

import "testing"

func TestComputeInsertIdx(t *testing.T) {
	cases := []struct {
		name       string
		sourceIdx  int
		rawTarget  int
		wantInsert int
		wantNoop   bool
	}{
		// Source at 0 in a list of 4. rawTarget covers 0..4.
		{"src0 raw0 noop", 0, 0, 0, true},
		{"src0 raw1 noop", 0, 1, 0, true},
		{"src0 raw2", 0, 2, 1, false},
		{"src0 raw3", 0, 3, 2, false},
		{"src0 raw4 (tail)", 0, 4, 3, false},

		// Source at 1.
		{"src1 raw0", 1, 0, 0, false},
		{"src1 raw1 noop", 1, 1, 0, true},
		{"src1 raw2 noop", 1, 2, 0, true},
		{"src1 raw3", 1, 3, 2, false},
		{"src1 raw4 (tail)", 1, 4, 3, false},

		// Source at 2.
		{"src2 raw0", 2, 0, 0, false},
		{"src2 raw1", 2, 1, 1, false},
		{"src2 raw2 noop", 2, 2, 0, true},
		{"src2 raw3 noop", 2, 3, 0, true},
		{"src2 raw4 (tail)", 2, 4, 3, false},

		// Source at 3 (last). Tail-drop is a no-op (already last).
		{"src3 raw0", 3, 0, 0, false},
		{"src3 raw1", 3, 1, 1, false},
		{"src3 raw2", 3, 2, 2, false},
		{"src3 raw3 noop", 3, 3, 0, true},
		{"src3 raw4 noop (tail)", 3, 4, 0, true},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			gotInsert, gotNoop := computeInsertIdx(tc.sourceIdx, tc.rawTarget)
			if gotNoop != tc.wantNoop {
				t.Fatalf("noop: got %v, want %v", gotNoop, tc.wantNoop)
			}
			if !tc.wantNoop && gotInsert != tc.wantInsert {
				t.Fatalf("insert: got %d, want %d", gotInsert, tc.wantInsert)
			}
		})
	}
}

func TestSlicesEqualInt64(t *testing.T) {
	cases := []struct {
		name string
		a, b []int64
		want bool
	}{
		{"both nil", nil, nil, true},
		{"both empty", []int64{}, []int64{}, true},
		{"nil vs empty", nil, []int64{}, true},
		{"single equal", []int64{42}, []int64{42}, true},
		{"single differ", []int64{42}, []int64{43}, false},
		{"equal multi", []int64{1, 2, 3}, []int64{1, 2, 3}, true},
		{"length mismatch", []int64{1, 2, 3}, []int64{1, 2}, false},
		{"reordered", []int64{1, 2, 3}, []int64{3, 2, 1}, false},
		{"off by one element", []int64{1, 2, 3}, []int64{1, 2, 4}, false},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			if got := slicesEqualInt64(tc.a, tc.b); got != tc.want {
				t.Fatalf("got %v, want %v", got, tc.want)
			}
		})
	}
}
