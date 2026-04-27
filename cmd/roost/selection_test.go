package main

import (
	"reflect"
	"testing"
)

func TestSelection_EmptyByDefault(t *testing.T) {
	var s selection
	if !s.empty() {
		t.Errorf("zero selection: empty() = false, want true")
	}
	if got := s.ribbonRects(80, 8, 16, 8, 8); got != nil {
		t.Errorf("zero selection: ribbonRects = %v, want nil", got)
	}
}

func TestSelection_SingleRowRibbon(t *testing.T) {
	var s selection
	s.start(2, 1)
	s.update(7, 1)

	got := s.ribbonRects(80, 10, 20, 8, 8)
	want := []ribbonRect{
		// row 1, cols 2..7 inclusive → eCol exclusive = 8 → width 6 cells
		{X: 8 + 2*10, Y: 8 + 1*20, W: 6 * 10, H: 20},
	}
	if !reflect.DeepEqual(got, want) {
		t.Errorf("single-row ribbon: got %v, want %v", got, want)
	}
}

func TestSelection_TwoRowRibbon(t *testing.T) {
	// Adjacent rows: no middle block, just first-row partial + last-row
	// partial.
	var s selection
	s.start(5, 0)
	s.update(3, 1)

	got := s.ribbonRects(80, 10, 20, 0, 0)
	want := []ribbonRect{
		// First row: cols 5..end → from x=50 to right edge (cols=80 → 80*10 - 50)
		{X: 50, Y: 0, W: float64(80-5) * 10, H: 20},
		// Last row: cols 0..3 inclusive → eCol exclusive = 4 → width 4
		{X: 0, Y: 20, W: 4 * 10, H: 20},
	}
	if !reflect.DeepEqual(got, want) {
		t.Errorf("two-row ribbon: got %v, want %v", got, want)
	}
}

func TestSelection_ThreeRowRibbon(t *testing.T) {
	var s selection
	s.start(10, 1)
	s.update(4, 3)

	got := s.ribbonRects(80, 10, 20, 0, 0)
	want := []ribbonRect{
		// First row (1): cols 10..end → x=100, w=(80-10)*10=700
		{X: 100, Y: 20, W: 700, H: 20},
		// Middle (row 2): full width
		{X: 0, Y: 40, W: 800, H: 20},
		// Last (row 3): cols 0..4 inclusive → eCol exclusive = 5
		{X: 0, Y: 60, W: 50, H: 20},
	}
	if !reflect.DeepEqual(got, want) {
		t.Errorf("three-row ribbon: got %v, want %v", got, want)
	}
}

func TestSelection_NormalizationSwapsBackwardsDrag(t *testing.T) {
	// User drags from (10, 5) up to (2, 1). Normalized form should
	// have start=(2,1), end=(10,5).
	var s selection
	s.start(10, 5)
	s.update(2, 1)
	sCol, sRow, eCol, eRow := s.normalized()
	if sCol != 2 || sRow != 1 || eCol != 10 || eRow != 5 {
		t.Errorf("normalized: got (%d,%d)..(%d,%d), want (2,1)..(10,5)",
			sCol, sRow, eCol, eRow)
	}
}

func TestSelection_TouchesRowRange(t *testing.T) {
	var s selection
	s.start(0, 5)
	s.update(10, 8)

	cases := []struct {
		min, max int
		want     bool
	}{
		{0, 4, false},  // entirely above
		{9, 12, false}, // entirely below
		{4, 5, true},   // overlaps top
		{8, 10, true},  // overlaps bottom
		{6, 7, true},   // entirely inside
		{0, 100, true}, // contains
		{5, 8, true},   // exact match
	}
	for _, c := range cases {
		if got := s.touches(c.min, c.max); got != c.want {
			t.Errorf("touches(%d,%d): got %v, want %v", c.min, c.max, got, c.want)
		}
	}
}

func TestSelection_ClearMakesEmpty(t *testing.T) {
	var s selection
	s.start(1, 1)
	s.update(5, 5)
	if s.empty() {
		t.Fatalf("pre-clear: empty() = true, want false")
	}
	s.clear()
	if !s.empty() {
		t.Errorf("post-clear: empty() = false, want true")
	}
}
