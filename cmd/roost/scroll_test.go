package main

import "testing"

// scrollAccumulator mirrors the math inside Session.handleScroll for
// the gdk.ScrollUnitSurface (smooth-scroll) path. Pure function so we
// can unit-test the accumulator without GTK in scope.
//
// Returns the integer rows to dispatch and the new accumulator value.
// dispatch is non-zero only when |accum+dy| crosses ±1.0; otherwise
// the delta is held in the accumulator until enough has built up.
func scrollAccumulator(accum, dy float64) (rows int, newAccum float64) {
	a := accum + dy
	switch {
	case a >= 1:
		rows = int(a)
		return rows, a - float64(rows)
	case a <= -1:
		rows = int(a)
		return rows, a - float64(rows)
	default:
		return 0, a
	}
}

func TestScrollAccumulator_SubLineDeltasAggregateToWholeRows(t *testing.T) {
	// 0.4 + 0.4 = 0.8 — under threshold; should aggregate.
	r1, a1 := scrollAccumulator(0, 0.4)
	if r1 != 0 || a1 != 0.4 {
		t.Errorf("first tick: got rows=%d accum=%v; want 0,0.4", r1, a1)
	}
	r2, a2 := scrollAccumulator(a1, 0.4)
	if r2 != 0 || a2 != 0.8 {
		t.Errorf("second tick: got rows=%d accum=%v; want 0,0.8", r2, a2)
	}
	// 0.8 + 0.4 = 1.2 — crosses threshold; dispatch 1, retain 0.2.
	r3, a3 := scrollAccumulator(a2, 0.4)
	if r3 != 1 {
		t.Errorf("third tick rows: got %d, want 1", r3)
	}
	if a3 < 0.19 || a3 > 0.21 {
		t.Errorf("third tick accum: got %v, want ~0.2", a3)
	}
}

func TestScrollAccumulator_NegativeDirection(t *testing.T) {
	// -0.6 + -0.6 = -1.2 — dispatches -1, retains -0.2.
	r1, a1 := scrollAccumulator(0, -0.6)
	if r1 != 0 {
		t.Errorf("first tick rows: got %d, want 0", r1)
	}
	if a1 != -0.6 {
		t.Errorf("first tick accum: got %v, want -0.6", a1)
	}
	r2, a2 := scrollAccumulator(a1, -0.6)
	if r2 != -1 {
		t.Errorf("second tick rows: got %d, want -1", r2)
	}
	if a2 < -0.21 || a2 > -0.19 {
		t.Errorf("second tick accum: got %v, want ~-0.2", a2)
	}
}

func TestScrollAccumulator_LargeDeltaDispatchesMultipleRows(t *testing.T) {
	// A trackpad fling could deliver dy=3.7 in one event.
	r, a := scrollAccumulator(0, 3.7)
	if r != 3 {
		t.Errorf("rows: got %d, want 3", r)
	}
	if a < 0.69 || a > 0.71 {
		t.Errorf("accum: got %v, want ~0.7", a)
	}
}
