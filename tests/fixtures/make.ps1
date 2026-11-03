# Makes the committed test fixtures with Office 2021 over COM, one family at a time:
#   powershell -NoProfile -File tests/fixtures/make.ps1 -Family word
# Office writes every file; features COM can't author are patched into the saved package and
# the file is opened and saved again by its app. Names are scrubbed last: document authors
# become doc-merge, comment authors Author A / Author B. Only the Office processes this script
# started are stopped.
param([Parameter(Mandatory)][ValidateSet('word')][string]$Family)
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing, System.IO.Compression, System.IO.Compression.FileSystem
$out = $PSScriptRoot
$tmp = [string](Join-Path ([IO.Path]::GetTempPath()) "doc-merge-fixtures-$PID")
New-Item -ItemType Directory -Force $tmp | Out-Null
$themes = 'C:\Program Files\Microsoft Office\root\Document Themes 16'
$script:mine = @()
function Step([string]$m) { Write-Host "$(Get-Date -Format HH:mm:ss) $m" }

# Starts an Office app and remembers the processes it started (also for Excel opened by a
# chart's data).
function Track([string[]]$names, [scriptblock]$start) {
  $before = @(Get-Process $names -ErrorAction SilentlyContinue | ForEach-Object Id)
  $result = & $start
  $script:mine += @(Get-Process $names -ErrorAction SilentlyContinue | Where-Object { $before -notcontains $_.Id } | ForEach-Object Id)
  $result
}

function Stop-Mine {
  foreach ($id in $script:mine) {
    if (Get-Process -Id $id -ErrorAction SilentlyContinue) { Stop-Process -Id $id -Force; Wait-Process -Id $id -Timeout 10 -ErrorAction SilentlyContinue }
  }
  $script:mine = @()
}

function New-Png([string]$path, [Drawing.Color]$color) {
  $b = New-Object Drawing.Bitmap 48, 48
  $g = [Drawing.Graphics]::FromImage($b)
  $g.Clear($color)
  $g.FillEllipse([Drawing.Brushes]::White, 12, 12, 24, 24)
  $g.Dispose()
  $b.Save($path, [Drawing.Imaging.ImageFormat]::Png)
  $b.Dispose()
}

# Rewrites the XML entries of a ZIP (a file, or bytes for an embedded package) through
# $edit(name, text), which returns the new text or $null to leave the entry as it is.
# Embedded Office packages are edited the same way.
function Edit-Zip($zip, [scriptblock]$edit) {
  foreach ($e in @($zip.Entries)) {
    $s = $e.Open(); $m = New-Object IO.MemoryStream; $s.CopyTo($m); $s.Dispose()
    $bytes = $m.ToArray()
    $new = $null
    if ($e.FullName -match '\.(xml|rels)$') {
      $text = [Text.Encoding]::UTF8.GetString($bytes).TrimStart([char]0xFEFF)
      $t = & $edit $e.FullName $text
      if ($null -ne $t -and $t -ne $text) { $new = [Text.Encoding]::UTF8.GetBytes($t) }
    } elseif ($e.FullName -match '\.(xlsx|docx|pptx)$') {
      $inner = New-Object IO.MemoryStream
      $inner.Write($bytes, 0, $bytes.Length)
      $z = New-Object IO.Compression.ZipArchive($inner, [IO.Compression.ZipArchiveMode]::Update, $true)
      Edit-Zip $z $edit
      $z.Dispose()
      $new = $inner.ToArray()
    }
    if ($null -ne $new) {
      $name = $e.FullName; $e.Delete()
      $s = $zip.CreateEntry($name).Open(); $s.Write($new, 0, $new.Length); $s.Dispose()
    }
  }
}

function Edit-Package([string]$path, [scriptblock]$edit) {
  $zip = [IO.Compression.ZipFile]::Open($path, [IO.Compression.ZipArchiveMode]::Update)
  try { Edit-Zip $zip $edit } finally { $zip.Dispose() }
}

# Document properties and stored paths carry the user's names; comment authors become $author.
function Scrub([string]$name, [string]$text, [string]$author) {
  switch -regex ($name) {
    'docProps/core\.xml$' {
      $text = $text -replace '<dc:creator>[^<]*</dc:creator>', '<dc:creator>doc-merge</dc:creator>'
      return $text -replace '<cp:lastModifiedBy>[^<]*</cp:lastModifiedBy>', '<cp:lastModifiedBy>doc-merge</cp:lastModifiedBy>'
    }
    'docProps/app\.xml$' { return $text -replace '<Company>[^<]*</Company>', '<Company></Company>' -replace '<Manager>[^<]*</Manager>', '' }
    'workbook\.xml$' { return $text -replace '<mc:AlternateContent[^>]*><mc:Choice Requires="x15">\s*<x15ac:absPath[^>]*/>\s*</mc:Choice></mc:AlternateContent>', '' }
    'comments\.xml$' {
      $initials = ($author -split ' ' | ForEach-Object { $_[0] }) -join ''
      return $text -replace 'w:author="[^"]*"', "w:author=""$author""" -replace 'w:initials="[^"]*"', "w:initials=""$initials"""
    }
    'people\.xml$' {
      return $text -replace '<w15:person w15:author="[^"]*">.*?</w15:person>', "<w15:person w15:author=""$author""><w15:presenceInfo w15:providerId=""None"" w15:userId=""$author""/></w15:person>"
    }
  }
  $null
}

function Word-Docs {
  $png = [string](Join-Path $tmp "picture.png"); New-Png $png ([Drawing.Color]::SteelBlue)
  $note = [string](Join-Path $tmp "note.png"); New-Png $note ([Drawing.Color]::IndianRed)
  $word = Track @('WINWORD') { New-Object -ComObject Word.Application }
  $word.Visible = $false
  $word.DisplayAlerts = 0
  try {
    # Puts the selection at the end of the main text (adding notes or comments moves it).
    function Tail { $doc.Characters.Last.Select(); $sel.Collapse(1) }
    # $style: a built-in style number (-1 Normal, -2 Heading 1) or a style name.
    function Para([string]$text, $style = -1) {
      if ("$style" -match '^-?\d+$') { $style = [int]$style }
      $sel.Style = $doc.Styles.Item($style); $sel.TypeText($text); $sel.TypeParagraph()
    }

    # a.docx: Calibri 11, blue Heading 1, red ClauseText, lists, first-page and default headers,
    # a PAGE footer, a footnote with a picture, a comment, bookmarks, a TOC, a chart, SmartArt,
    # a picture and a landscape last section.
    $doc = $word.Documents.Add(); $sel = $word.Selection
    $doc.ApplyDocumentTheme("$themes\Office 2013 - 2022 Theme.thmx")
    $doc.Styles.Item(-1).Font.Size = 11
    $clause = $doc.Styles.Add('ClauseText', 1); $clause.BaseStyle = -1; $clause.Font.Color = 255; $clause.Font.Size = 11
    Para 'Document A' -1
    $tocAt = $sel.Range.Start
    Para '' -1
    Para 'Introduction' -2
    $start = $sel.Range.Start; $sel.TypeText('Intro text of A.')
    $doc.Bookmarks.Add('Intro', $doc.Range($start, $sel.Range.Start)) | Out-Null
    $sel.TypeParagraph()
    Para 'Clause A one.' 'ClauseText'
    Step list
    $listAt = $sel.Range.Start
    Para 'A first' -1; Para 'A second' -1; Para 'A third' -1
    $doc.Range($listAt, $sel.Range.Start - 1).ListFormat.ApplyNumberDefault()
    $sel.Range.ListFormat.RemoveNumbers()
    $bulletAt = $sel.Range.Start
    Para 'A bullet' -1; Para 'Another bullet' -1
    $doc.Range($bulletAt, $sel.Range.Start - 1).ListFormat.ApplyBulletDefault()
    $sel.Range.ListFormat.RemoveNumbers()
    Step picture
    $sel.TypeText("A picture: ")
    $sel.InlineShapes.AddPicture($png) | Out-Null
    $sel.TypeParagraph()
    $sel.TypeText('Text with a note')
    Step footnote
    $fn = $doc.Footnotes.Add($sel.Range)
    Tail
    $fn.Range.Text = 'A footnote with a picture '
    $r = $fn.Range; $r.Collapse(0); $r.InlineShapes.AddPicture($note) | Out-Null
    $sel.TypeText(' and a comment.')
    $doc.Comments.Add($doc.Range($sel.Range.Start - 8, $sel.Range.Start - 1), 'A comment by A.') | Out-Null
    Tail
    $sel.TypeParagraph()
    Step chart
    $chart = Track @('EXCEL') { $sel.InlineShapes.AddChart2(-1, 51) }
    try { $chart.Chart.ChartData.Workbook.Application.Quit() } catch {}
    Tail
    $sel.TypeParagraph()
    Step smartart
    $sel.InlineShapes.AddSmartArt($word.SmartArtLayouts.Item(1)) | Out-Null
    Tail
    $sel.TypeParagraph()
    Para 'Second part' -2
    Para 'More text of A.' -1
    Step section
    $sel.InsertBreak(2)
    Para 'A landscape page.' -1
    $sel.TypeText('The end of A.')
    $doc.Sections.Last.PageSetup.Orientation = 1
    $s1 = $doc.Sections.Item(1)
    Step headers
        $s1.PageSetup.DifferentFirstPageHeaderFooter = -1
    $s1.Headers.Item(1).Range.Text = 'A header'
    $s1.Headers.Item(2).Range.Text = 'A first page header'
    $f = $s1.Footers.Item(1).Range
    $f.Text = 'Page '; $f.Collapse(0)
    $f.Fields.Add($f, 33) | Out-Null
    Step toc
    $doc.TablesOfContents.Add($doc.Range($tocAt, $tocAt), $true, 1, 3) | Out-Null
    Step save
    $doc.SaveAs2([string](Join-Path $out 'a.docx'), 12)
    $doc.Close(0)

    # b.docx: another theme and Aptos 12, blue 14 pt ClauseText, green Heading 1, a list,
    # odd and even headers, an endnote, a comment by another author, a bookmark Intro with a
    # REF field to it, a content control bound to the title, and a.docx's picture.
    $doc = $word.Documents.Add(); $sel = $word.Selection
    $doc.ApplyDocumentTheme("$themes\Office Theme.thmx")
    $clause = $doc.Styles.Add('ClauseText', 1); $clause.BaseStyle = -1; $clause.Font.Color = 16711680; $clause.Font.Size = 14
    $doc.Styles.Item(-2).Font.Color = 32768
    $sel.TypeParagraph()
    $cc = $doc.ContentControls.Add(1, $doc.Range(0, 0))
    $core = $doc.CustomXMLParts.SelectByNamespace('http://schemas.openxmlformats.org/package/2006/metadata/core-properties').Item(1)
    $cc.XMLMapping.SetMapping('/ns1:coreProperties[1]/ns0:title[1]', "xmlns:ns0='http://purl.org/dc/elements/1.1/' xmlns:ns1='http://schemas.openxmlformats.org/package/2006/metadata/core-properties'", $core) | Out-Null
    $cc.Range.Text = 'Title of B'
    Tail
    Para 'Introduction' -2
    $start = $sel.Range.Start; $sel.TypeText('Intro text of B.')
    $doc.Bookmarks.Add('Intro', $doc.Range($start, $sel.Range.Start)) | Out-Null
    $sel.TypeParagraph()
    $sel.TypeText('See: ')
    $sel.Fields.Add($sel.Range, 3, 'Intro', $false) | Out-Null
    $sel.TypeParagraph()
    Para 'Clause B one.' 'ClauseText'
    Step list
    $listAt = $sel.Range.Start
    Para 'B first' -1; Para 'B second' -1; Para 'B third' -1
    $doc.Range($listAt, $sel.Range.Start - 1).ListFormat.ApplyNumberDefault()
    $sel.Range.ListFormat.RemoveNumbers()
    $sel.TypeText('The same picture: ')
    $sel.InlineShapes.AddPicture($png) | Out-Null
    $sel.TypeParagraph()
    $sel.TypeText('Text with an endnote')
    $doc.Endnotes.Add($sel.Range).Range.Text = 'An endnote of B.'
    Tail
    $sel.TypeText(' and a comment.')
    $doc.Comments.Add($doc.Range($sel.Range.Start - 8, $sel.Range.Start - 1), 'A comment by B.') | Out-Null
    Tail
    $sel.TypeParagraph()
    $sel.InsertBreak(7)
    Para 'Page two of B.' -1
    $sel.TypeText('Clause B two.')
    $sel.Style = 'ClauseText'
    $s1 = $doc.Sections.Item(1)
    $s1.PageSetup.OddAndEvenPagesHeaderFooter = -1
    $s1.Headers.Item(1).Range.Text = 'B odd header'
    $s1.Headers.Item(3).Range.Text = 'B even header'
    Step save
    $doc.SaveAs2([string](Join-Path $out 'b.docx'), 12)
    $doc.Close(0)

    # c.docx: no headers or footers, a table at the end, Word 2010 layout mode.
    $doc = $word.Documents.Add(); $sel = $word.Selection
    $doc.ApplyDocumentTheme("$themes\Office 2013 - 2022 Theme.thmx")
    $doc.Styles.Item(-1).Font.Size = 11
    Para 'Document C' -1
    $sel.TypeText('C ends with a table.')
    $sel.TypeParagraph()
    $t = $doc.Tables.Add($sel.Range, 2, 2)
    $t.Borders.Enable = -1
    $t.Cell(1, 1).Range.Text = 'C1'; $t.Cell(1, 2).Range.Text = 'C2'
    $t.Cell(2, 1).Range.Text = 'C3'; $t.Cell(2, 2).Range.Text = 'C4'
    Step save
    $doc.SaveAs2([string](Join-Path $out 'c.docx'), 12, $false, '', $true, '', $false, $false, $false, $false, $false, 65001, $false, $false, 0, $false, 14)
    $doc.Close(0)

    # Word's own settings for a and c, Aptos 12 for b: patched in, then saved by Word again.
    $defaults = @{
      a = @(22, 259); c = @(22, 259); b = @(24, 278)
    }
    foreach ($n in 'a', 'b', 'c') {
      $sz = $defaults[$n][0]; $line = $defaults[$n][1]
      $dd = "<w:docDefaults><w:rPrDefault><w:rPr><w:rFonts w:asciiTheme=""minorHAnsi"" w:eastAsiaTheme=""minorHAnsi"" w:hAnsiTheme=""minorHAnsi"" w:cstheme=""minorBidi""/><w:sz w:val=""$sz""/><w:szCs w:val=""$sz""/><w:lang w:val=""en-US"" w:eastAsia=""en-US"" w:bidi=""ar-SA""/></w:rPr></w:rPrDefault><w:pPrDefault><w:pPr><w:spacing w:after=""160"" w:line=""$line"" w:lineRule=""auto""/></w:pPr></w:pPrDefault></w:docDefaults>"
      $author = if ($n -eq 'b') { 'Author B' } else { 'Author A' }
      Edit-Package (Join-Path $out "$n.docx") {
        param($name, $text)
        if ($name -eq 'word/styles.xml') { return $text -replace '<w:docDefaults>.*?</w:docDefaults>', $dd }
        Scrub $name $text $author
      }.GetNewClosure()
      Step resave
      $doc = $word.Documents.Open([string](Join-Path $out "$n.docx"))
      $doc.Saved = $false
      $doc.Save()
      $doc.Close(0)
    }
  } finally {
    try { $word.Quit() } catch {}
    Start-Sleep -Seconds 2
    Stop-Mine
  }
  # Last, after Word's final save: names in the properties again, and c's closing paragraph
  # (Word always adds one after a final table) removed so its body ends in the table.
  foreach ($n in 'a', 'b', 'c') {
    $author = if ($n -eq 'b') { 'Author B' } else { 'Author A' }
    Edit-Package (Join-Path $out "$n.docx") {
      param($name, $text)
      if ($n -eq 'c' -and $name -eq 'word/document.xml') {
        return $text -replace '</w:tbl><w:p\b(?:(?!</w:p>).)*?</w:p>(<w:sectPr)', '</w:tbl>$1' -replace '</w:tbl><w:p\b[^>]*/>(<w:sectPr)', '</w:tbl>$1'
      }
      Scrub $name $text $author
    }.GetNewClosure()
  }
}

try {
  switch ($Family) { 'word' { Word-Docs } }
} catch {
  $_.InvocationInfo.PositionMessage
  throw
} finally {
  Stop-Mine
  Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}
Get-ChildItem $out -Filter *.docx | ForEach-Object { "{0} {1:N0} bytes" -f $_.Name, $_.Length }
