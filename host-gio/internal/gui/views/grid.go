package views

import (
	"image"
	"image/color"
	"math"

	"gioui.org/f32"
	"gioui.org/font"
	"gioui.org/layout"
	"gioui.org/op"
	"gioui.org/op/clip"
	"gioui.org/op/paint"
	"gioui.org/text"
	"gioui.org/unit"
	"gioui.org/widget/material"
	"golang.org/x/image/math/fixed"
)

var ShouldDrawRulers = true

type Grid struct {
	// constructor properties
	shaper   *text.Shaper
	face     font.Typeface
	textSize unit.Sp

	// lazily computed proeprties
	charW int
	charH int

	// public properties
	AnchorColor color.NRGBA
	AnchorCols  int
	AnchorRows  int
}

type GridChild struct {
	rect   image.Rectangle
	widget layout.Widget
}

func Cell(rect image.Rectangle, widget layout.Widget) GridChild {
	return GridChild{
		rect:   rect,
		widget: widget,
	}
}

func NewGrid(th *material.Theme) *Grid {
	return &Grid{
		shaper:   th.Shaper,
		face:     th.Face,
		textSize: th.TextSize,
	}
}

func (g *Grid) measure(metric unit.Metric) {
	pxPerEm := fixed.Int26_6(float32(g.textSize) * metric.PxPerSp * 64)
	g.shaper.LayoutString(text.Parameters{
		Font:      font.Font{Typeface: g.face},
		PxPerEm:   pxPerEm,
		MaxWidth:  math.MaxInt,
		MinWidth:  0,
		Alignment: text.Start,
	}, "M")
	for {
		glyph, ok := g.shaper.NextGlyph()
		if !ok {
			break
		}
		g.charW = glyph.Advance.Ceil()
		g.charH = (glyph.Ascent + glyph.Descent).Ceil()
	}
}

func (g *Grid) Layout(gtx layout.Context, children ...GridChild) layout.Dimensions {
	if g.charW == 0 {
		g.measure(gtx.Metric)
	}

	w := gtx.Constraints.Max.X
	h := gtx.Constraints.Max.Y

	// Clip to view bounds
	clipStack := clip.Rect(image.Rect(0, 0, w, h)).Push(gtx.Ops)
	defer clipStack.Pop()

	// Black background
	paint.ColorOp{Color: color.NRGBA{A: 255}}.Add(gtx.Ops)
	paint.PaintOp{}.Add(gtx.Ops)

	// Draw anchors (crosses) at grid intersections
	if g.AnchorColor.A > 0 && g.AnchorCols > 0 && g.AnchorRows > 0 {
		cols := w / g.charW
		rows := h / g.charH

		paint.ColorOp{Color: g.AnchorColor}.Add(gtx.Ops)

		for c := 0; c <= cols; c += g.AnchorCols {
			for r := 0; r <= rows; r += g.AnchorRows {
				x := float32(c * g.charW)
				y := float32(r * g.charH)
				// Horizontal line of cross: 10px wide, 1px tall, centered on (x,y)
				var hPath clip.Path
				hPath.Begin(gtx.Ops)
				hPath.MoveTo(f32.Point{X: x - 5, Y: y})
				hPath.LineTo(f32.Point{X: x + 5, Y: y})
				hStroke := clip.Stroke{Path: hPath.End(), Width: 1}
				hPush := hStroke.Op().Push(gtx.Ops)
				paint.PaintOp{}.Add(gtx.Ops)
				hPush.Pop()
				// Vertical line of cross: 1px wide, 10px tall, centered on (x,y)
				var vPath clip.Path
				vPath.Begin(gtx.Ops)
				vPath.MoveTo(f32.Point{X: x, Y: y - 5})
				vPath.LineTo(f32.Point{X: x, Y: y + 5})
				vStroke := clip.Stroke{Path: vPath.End(), Width: 1}
				vPush := vStroke.Op().Push(gtx.Ops)
				paint.PaintOp{}.Add(gtx.Ops)
				vPush.Pop()
			}
		}
	}

	// Draw children
	for _, child := range children {
		x := child.rect.Min.X * g.charW
		y := child.rect.Min.Y * g.charH
		w := child.rect.Dx() * g.charW
		h := child.rect.Dy() * g.charH

		stack := op.Offset(image.Pt(x, y)).Push(gtx.Ops)
		childGtx := gtx
		childGtx.Constraints = layout.Exact(image.Pt(w, h))
		child.widget(childGtx)
		stack.Pop()
	}

	return layout.Dimensions{Size: gtx.Constraints.Max}
}
