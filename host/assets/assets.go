// Package assets holds embedded static files (icons, images, fonts, etc.).
package assets

import (
	_ "embed"
)

//go:embed fonts/FiraCodeNerdFontMono-Regular.ttf
var FiraCodeTTF []byte

//go:embed fonts/FiraCodeNerdFontMono-Bold.ttf
var FiraCodeBoldTTF []byte
