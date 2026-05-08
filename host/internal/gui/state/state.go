package state

import (
	"gioui.org/font"
	"gioui.org/font/opentype"
	"gioui.org/layout"
	"gioui.org/text"
	"gioui.org/unit"
	"gioui.org/widget/material"
	"github.com/fpvladder/laps/host/assets"
)

const (
	viewW    = 2560
	viewH    = 1440
	textSize = unit.Sp(13)
)

type State interface {
	Enter()
	Layout(gtx layout.Context) layout.Dimensions
	Exit()
}

type Machine struct {
	State State
}

func NewMachine(invalidate func()) *Machine {
	faceRegular, err := opentype.Parse(assets.FiraCodeTTF)
	if err != nil {
		panic(err)
	}
	faceBold, err := opentype.Parse(assets.FiraCodeBoldTTF)
	if err != nil {
		panic(err)
	}

	regularFont := faceRegular.Font()
	boldFont := faceBold.Font()
	boldFont.Weight = font.Bold

	th := &material.Theme{
		Shaper: text.NewShaper(
			text.WithCollection(
				[]font.FontFace{
					{Font: regularFont, Face: faceRegular},
					{Font: boldFont, Face: faceBold},
				},
			),
		),
		Face:     regularFont.Typeface,
		TextSize: textSize,
	}

	s := NewOnlyState(th, invalidate)
	return &Machine{State: s}
}
