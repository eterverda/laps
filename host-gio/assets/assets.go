// Package assets holds embedded static files (icons, images, fonts, etc.).
package assets

import (
	_ "embed"
)

//go:generate go run gioui.org/cmd/svg2gio -pkg assets -o testcard.go images/testcard.svg

//go:embed fonts/FiraCodeNerdFontMono-Regular.ttf
var FiraCodeTTF []byte

//go:embed fonts/FiraCodeNerdFontMono-Bold.ttf
var FiraCodeBoldTTF []byte
