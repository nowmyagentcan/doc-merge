# Makes the committed test fixtures with Office 2021 over COM, one family at a time:
#   powershell -NoProfile -File tests/fixtures/make.ps1 -Family word
# Office writes every file; features COM can't author are patched into the saved package and
# the file is opened and saved again by its app. Names are scrubbed last: document authors
# become doc-merge, comment authors Author A / Author B. Only the Office processes this script
# started are stopped.
param([Parameter(Mandatory)][ValidateSet('word', 'excel', 'powerpoint')][string]$Family)
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
    'xl/comments\d*\.xml$' { return $text -replace '<author>(?!tc=)[^<]*</author>', "<author>$author</author>" }
    'xl/persons/person\.xml$' {
      return $text -replace 'displayName="[^"]*"', "displayName=""$author""" -replace 'userId="[^"]*"', "userId=""$author""" -replace 'providerId="[^"]*"', 'providerId="None"'
    }
    'ppt/authors\.xml$' {
      return $text -replace ' id="\{[^"]*"', ' id="{7B2E9C41-0F3A-4D6E-8A11-5C0D2E4F6A01}"' -replace 'name="[^"]*"', "name=""$author""" -replace 'initials="[^"]*"', 'initials="AA"' -replace 'userId="[^"]*"', "userId=""$author""" -replace 'providerId="[^"]*"', 'providerId="None"'
    }
    'ppt/comments/modernComment[^/]*\.xml$' { return $text -replace 'authorId="[^"]*"', 'authorId="{7B2E9C41-0F3A-4D6E-8A11-5C0D2E4F6A01}"' }
    'pivotCacheDefinition\d*\.xml$' { return $text -replace 'refreshedBy="[^"]*"', 'refreshedBy="doc-merge"' }
    'externalLink\d+\.xml\.rels$' { return $text -replace 'Target="[^"]*ext\.xlsx"', 'Target="ext.xlsx"' }
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

function Excel-Books {
  $none = [Type]::Missing
  $xl = Track @('EXCEL') { New-Object -ComObject Excel.Application }
  $xl.Visible = $false
  $xl.DisplayAlerts = $false
  $xl.AskToUpdateLinks = $false
  try {
    # Fills $sheet from row $row with the rows of $table (arrays of cell values).
    function Rows($sheet, [int]$row, [object[]]$table) {
      foreach ($line in $table) {
        for ($c = 0; $c -lt $line.Count; $c++) { $sheet.Cells.Item($row, $c + 1).Formula = [string]$line[$c] }
        $row++
      }
    }

    # a.xlsx: sheets Data and Summary, a custom number format, the name Total, the table
    # Sales, a chart, a conditional format, a validation, a note and a threaded comment.
    $wb = $xl.Workbooks.Add()
    $wb.ApplyTheme("$themes\Office 2013 - 2022 Theme.thmx")
    $data = $wb.Worksheets.Item(1); $data.Name = 'Data'
    Rows $data 1 @(@('Item', 'Amount', 'Qty'), @('Apple', 12.5, 3), @('Pear', 30, 4), @('Plum', 7.25, 9), @('Fig', 41, 2))
    $data.ListObjects.Add(1, $data.Range('A1:C5'), $none, 1).Name = 'Sales'
    $data.Range('B2:B5').NumberFormat = '0.00" kg"'
    Step chart
    $chart = $data.Shapes.AddChart2(201, 51, 320, 10, 280, 180)
    $chart.Chart.SetSourceData($data.Range('A1:B5'))
    $cf = $data.Range('B2:B5').FormatConditions.Add(1, 5, '=25')
    $cf.Interior.Color = 0xCEC7FF; $cf.Font.Bold = $true
    $data.Range('E2').Validation.Add(3, 1, 1, 'Apple,Pear,Plum') | Out-Null
    $data.Range('E1').Value2 = 'Pick'
    $summary = $wb.Worksheets.Add($none, $data); $summary.Name = 'Summary'
    Rows $summary 1 @(@(1, 'first'), @('Total', 0))
    $summary.Range('B2').Formula = '=SUM(Sales[Amount])'
    $wb.Names.Add('Total', '=Summary!$B$2') | Out-Null
    $summary.Range('C2').Formula = '=Total*3'
    $summary.Range('C3').Formula = '=Data!B2+1'
    $summary.Range('D2').AddComment('A note by A.') | Out-Null
    Step threaded
    $summary.Range('D3').AddCommentThreaded('A threaded comment by A.') | Out-Null
    $data.Activate()
    $wb.SaveAs([string](Join-Path $out 'a.xlsx'), 51)
    $wb.Close($false)

    # The workbook b.xlsx links to.
    $wb = $xl.Workbooks.Add()
    $wb.Worksheets.Item(1).Range('A1').Value2 = 99
    $ext = [string](Join-Path $tmp 'ext.xlsx')
    $wb.SaveAs($ext, 51)
    $wb.Close($false)

    # b.xlsx: another theme with theme colours in fills, fonts and the tab, the clashing sheet
    # Summary, table Sales, name Total and number format id, the formulas of the merge cases,
    # sparklines, a spilling FILTER, a check box, an external link, and each kind of formula
    # text: a conditional format, a validation, a hyperlink, a calculated column, a local name.
    $wb = $xl.Workbooks.Add()
    $wb.ApplyTheme("$themes\Facet.thmx")
    $summary = $wb.Worksheets.Item(1); $summary.Name = 'Summary'
    Rows $summary 1 @(@(10, 'one'), @(20, 7.5), @(30, 'plain'))
    $summary.Range('B2').NumberFormat = '#,##0.0" units"'
    $summary.Range('A1:B1').Interior.ThemeColor = 6
    $summary.Range('A1:B1').Interior.TintAndShade = 0.6
    $summary.Range('A2').Interior.ThemeColor = 9
    $summary.Range('A1').Font.ThemeColor = 5
    $summary.Range('B1').Font.ThemeColor = 10
    $summary.Range('B1').Font.TintAndShade = -0.25
    $summary.Range('B2').Interior.ThemeColor = 4
    $summary.Range('A3').Font.Name = 'Consolas'
    $summary.Range('A3').Font.Color = 0x0000FF
    $summary.Tab.ThemeColor = 7
    $detail = $wb.Worksheets.Add($none, $summary); $detail.Name = 'Detail'
    $detail.Range('A1').Value2 = 5
    Rows $detail 3 @(@('Item', 'Amount', 'Double'), @('Rope', 4, 0), @('Nail', 1, 0), @('Bolt', 6, 0))
    $list = $detail.ListObjects.Add(1, $detail.Range('A3:C6'), $none, 1); $list.Name = 'Sales'
    $list.ListColumns.Item(3).DataBodyRange.Formula = '=[@Amount]*2'
    $list.ShowTotals = $true
    $wb.Names.Add('Total', '=Detail!$B$4') | Out-Null
    $detail.Names.Add('Local', '=Summary!$A$2') | Out-Null
    $detail.Range('E1').Formula = '=Summary!B2'
    $detail.Range('E2').Formula = '=SUM(Sales[Amount])'
    $detail.Range('E3').Formula = '=Total*2'
    $detail.Range('E4').Formula = '=SUM(Detail:Summary!A1)'
    $detail.Range('E5').Formula = '=Local+1'
    $detail.Range('E6').Formula = "='" + $tmp + "\[ext.xlsx]Sheet1'!`$A`$1"
    Step sparklines
    $detail.Range('F4').SparklineGroups.Add(1, 'Detail!B4:B6') | Out-Null
    $detail.Range('F4').SparklineGroups.Item(1).SeriesColor.ThemeColor = 6
    $detail.Range('G1').Formula2 = '=FILTER(Sales[Item],Sales[Amount]>2)'
    Step controls
    $box = $detail.CheckBoxes().Add(420, 10, 90, 18)
    $box.LinkedCell = 'Summary!$D$1'
    $box.Value = 1
    $cf = $detail.Range('B4:B6').FormatConditions.Add(2, 0, '=B4>Summary!$A$1/5')
    $cf.Interior.Color = 0x9CE6FF; $cf.Font.Italic = $true
    $cf = $detail.Range('A1').FormatConditions.Add(1, 5, '=3')
    $cf.Font.ThemeColor = 6; $cf.Font.Bold = $true
    $detail.Range('H1').Validation.Add(3, 1, 1, '=Summary!$A$1:$A$3') | Out-Null
    $detail.Hyperlinks.Add($detail.Range('H2'), '', 'Summary!A1', '', 'Back') | Out-Null
    $summary.PageSetup.PrintArea = '$A$1:$B$3'
    $summary.Activate()
    $wb.SaveAs([string](Join-Path $out 'b.xlsx'), 51)
    $wb.Close($false)

    # c.xlsx: the 1904 date system.
    $wb = $xl.Workbooks.Add()
    $wb.Date1904 = $true
    $s = $wb.Worksheets.Item(1); $s.Name = 'Dates'
    $s.Range('A1').Formula = '=DATE(2024,1,2)'
    $s.Range('A1').NumberFormat = 'yyyy-mm-dd'
    $wb.SaveAs([string](Join-Path $out 'c.xlsx'), 51)
    $wb.Close($false)

    # p.xlsx: a pivot table.
    $wb = $xl.Workbooks.Add()
    $s = $wb.Worksheets.Item(1); $s.Name = 'Facts'
    Rows $s 1 @(@('Item', 'Amount'), @('Apple', 3), @('Pear', 4), @('Apple', 5))
    $report = $wb.Worksheets.Add($none, $s); $report.Name = 'Report'
    $pivot = $wb.PivotCaches().Create(1, 'Facts!R1C1:R4C2').CreatePivotTable('Report!R3C1', 'Pivot1')
    $pivot.PivotFields('Item').Orientation = 1
    $pivot.AddDataField($pivot.PivotFields('Amount')) | Out-Null
    $wb.SaveAs([string](Join-Path $out 'p.xlsx'), 51)
    $wb.Close($false)
  } finally {
    try { $xl.Quit() } catch {}
    Start-Sleep -Seconds 2
    Stop-Mine
  }
  foreach ($n in 'a', 'b', 'c', 'p') {
    $author = if ($n -eq 'b') { 'Author B' } else { 'Author A' }
    Edit-Package (Join-Path $out "$n.xlsx") { param($name, $text) Scrub $name $text $author }.GetNewClosure()
  }
}

# A font made for these tests: every letter and digit is one box, so no licence applies.
$boxFont = 'AAEAAAAKAIAAAwAgT1MvMkT8RAoAAAEoAAAAYGNtYXABzwKoAAABlAAAANZnbHlmTjxOOgAAAnQAAAA0aGVhZC+QkB4AAACsAAAANmhoZWEFegH4AAAA5AAAACRobXR4BdwAyAAAAYgAAAAMbG9jYQANACcAAAJsAAAACG1heHAABQAGAAABCAAAACBuYW1lqT2x+QAAAqgAAAFKcG9zdANqcHoAAAP0AAAALAABAAAAAQAArEgCkV8PPPUAAwPoAAAAAOcRJkEAAAAA5xEmQQBkAAAB9AK8AAAAAwACAAAAAAAAAAEAAAMg/zgAAAJYAGQAZAH0AAEAAAAAAAAAAAAAAAAAAAADAAEAAAADAAQAAQAAAAAAAgAAAAAAAAAAAAAAAAAAAAAAAwH0AZAABQAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABAAAAAAAAAAAAAAAAPz8/PwAAACAAegMg/zgAAAMgAMgAAAAAAAAAAAAAAAAAAAAgAAACWABkASwAAAJYAGQAAAACAAAAAwAAABQAAwABAAAAFAAEAMIAAAAMAAgAAgAEACAALgA5AFoAev//AAAAIAAsADAAQQBh////4QAAAAAAAAAAAAEAAAAKAA4AIABSAAAAAgACAAIAAgACAAIAAgACAAIAAgACAAIAAgACAAIAAgACAAIAAgACAAIAAgACAAIAAgACAAIAAgACAAIAAgACAAIAAgACAAIAAgACAAIAAgACAAIAAgACAAIAAgACAAIAAgACAAIAAgACAAIAAgACAAIAAgACAAIAAgACAAIAAgACAAAAAAANAA0AGgABAGQAAAH0ArwAAwAAMxEhEWQBkAK8/UQAAAEAZAAAAfQCvAADAAAzESERZAGQArz9RAAAAAAMAJYAAQAAAAAAAQAJAAAAAQAAAAAAAgAHAAkAAQAAAAAAAwARABAAAQAAAAAABAAJAAAAAQAAAAAABQALACEAAQAAAAAABgAQACwAAwABBAkAAQASADwAAwABBAkAAgAOAE4AAwABBAkAAwAiAFwAAwABBAkABAASADwAAwABBAkABQAWAH4AAwABBAkABgAgAJRNZXJnZSBCb3hSZWd1bGFyTWVyZ2UgQm94IFJlZ3VsYXJWZXJzaW9uIDEuME1lcmdlQm94LVJlZ3VsYXIATQBlAHIAZwBlACAAQgBvAHgAUgBlAGcAdQBsAGEAcgBNAGUAcgBnAGUAIABCAG8AeAAgAFIAZQBnAHUAbABhAHIAVgBlAHIAcwBpAG8AbgAgADEALgAwAE0AZQByAGcAZQBCAG8AeAAtAFIAZQBnAHUAbABhAHIAAAACAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAMAAAADAQIDYm94'

Add-Type -Namespace Native -Name Gdi -MemberDefinition @'
[DllImport("gdi32.dll", CharSet = CharSet.Unicode)] public static extern int AddFontResourceW(string file);
[DllImport("gdi32.dll", CharSet = CharSet.Unicode)] public static extern bool RemoveFontResourceW(string file);
'@

# A new deck from a template, without the blank slide it opens with.
function From-Template($ppt, [string]$template, [int]$window) {
  $deck = $ppt.Presentations.Open($template, 0, -1, $window)
  while ($deck.Slides.Count) { $deck.Slides.Item(1).Delete() }
  $deck
}

# Adds a slide with a title and returns it.
function Add-Slide($deck, [int]$layout, [string]$title) {
  $s = $deck.Slides.AddSlide($deck.Slides.Count + 1, $deck.SlideMaster.CustomLayouts.Item($layout))
  $s.Shapes.Title.TextFrame.TextRange.Text = $title
  $s
}

function Add-Text($slide, [string]$text, [int]$left, [int]$top) {
  $box = $slide.Shapes.AddTextbox(1, $left, $top, 300, 40)
  $box.TextFrame.TextRange.Text = $text
  $box
}

function Add-Notes($slide, [string]$text) {
  $slide.NotesPage.Shapes.Placeholders.Item(2).TextFrame.TextRange.Text = $text
}

# Opens a patched file and lets PowerPoint save it, embedding fonts when $fonts is -1.
function Save-Again($ppt, [string]$path, [int]$fonts) {
  $d = $ppt.Presentations.Open($path, 0, 0, 0)
  $copy = [string](Join-Path $tmp (Split-Path $path -Leaf))
  $d.SaveAs($copy, 24, $fonts)
  $d.Close()
  Copy-Item $copy $path -Force
}

# Makes a text box jump to $target when clicked.
function Add-Jump($slide, $target, [string]$text) {
  $box = Add-Text $slide $text 60 400
  $title = $target.Shapes.Title.TextFrame.TextRange.Text
  $box.ActionSettings.Item(1).Hyperlink.SubAddress = "$($target.SlideID),$($target.SlideIndex),$title"
}

# An action button that runs the custom show called $show (10 = ppActionNamedSlideShow).
function Add-ShowButton($slide, [string]$show) {
  $button = $slide.Shapes.AddShape(125, 60, 300, 120, 60)
  $button.ActionSettings.Item(1).Action = 10
  $button.ActionSettings.Item(1).SlideShowName = $show
}

# Adds a legacy comment by Author A to a slide: PowerPoint here writes only modern ones when
# asked, but keeps legacy ones it finds in a file.
function Add-LegacyComment([string]$path, [string]$slide) {
  $p = 'xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"'
  $declaration = '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>' + "`r`n"
  $office = 'http://schemas.openxmlformats.org/officeDocument/2006/relationships'
  $zip = [IO.Compression.ZipFile]::Open($path, [IO.Compression.ZipArchiveMode]::Update)
  try {
    $read = { param($n) $s = New-Object IO.StreamReader($zip.GetEntry($n).Open()); $t = $s.ReadToEnd(); $s.Dispose(); $t }
    $put = {
      param($n, $t)
      $old = $zip.GetEntry($n); if ($old) { $old.Delete() }
      $s = New-Object IO.StreamWriter($zip.CreateEntry($n).Open(), (New-Object Text.UTF8Encoding($false))); $s.Write($t); $s.Dispose()
    }
    & $put 'ppt/commentAuthors.xml' ($declaration + "<p:cmAuthorLst $p><p:cmAuthor id=""1"" name=""Author A"" initials=""AA"" lastIdx=""1"" clrIdx=""0""/></p:cmAuthorLst>")
    & $put 'ppt/comments/comment1.xml' ($declaration + "<p:cmLst $p><p:cm authorId=""1"" dt=""2026-11-04T10:00:00.000"" idx=""1""><p:pos x=""80"" y=""80""/><p:text>Comment on B</p:text></p:cm></p:cmLst>")
    $rels = $slide -replace '^(.*)/([^/]+)$', '$1/_rels/$2.rels'
    & $put $rels ((& $read $rels) -replace '</Relationships>', "<Relationship Id=""rId90"" Type=""$office/comments"" Target=""../comments/comment1.xml""/></Relationships>")
    $rels = 'ppt/_rels/presentation.xml.rels'
    & $put $rels ((& $read $rels) -replace '</Relationships>', "<Relationship Id=""rId90"" Type=""$office/commentAuthors"" Target=""commentAuthors.xml""/></Relationships>")
    $types = 'application/vnd.openxmlformats-officedocument.presentationml'
    & $put '[Content_Types].xml' ((& $read '[Content_Types].xml') -replace '</Types>', "<Override PartName=""/ppt/commentAuthors.xml"" ContentType=""$types.commentAuthors+xml""/><Override PartName=""/ppt/comments/comment1.xml"" ContentType=""$types.comments+xml""/></Types>")
  } finally { $zip.Dispose() }
}

function Ppt-Decks {
  if (Get-Process POWERPNT -ErrorAction SilentlyContinue) { throw 'PowerPoint is running: it has one instance, so wait for it to end.' }
  $png = [string](Join-Path $tmp 'picture.png'); New-Png $png ([Drawing.Color]::SteelBlue)
  $ttf = [string](Join-Path $tmp 'box.ttf'); [IO.File]::WriteAllBytes($ttf, [Convert]::FromBase64String($boxFont))
  if ([Native.Gdi]::AddFontResourceW($ttf) -eq 0) { throw 'The test font did not load.' }
  $ppt = Track @('POWERPNT') { New-Object -ComObject PowerPoint.Application }
  $template = [string](Join-Path $tmp 'template.potx')
  $saved = @{ a = [string](Join-Path $out 'a.pptx'); b = [string](Join-Path $out 'b.pptx'); c = [string](Join-Path $out 'c.pptx') }
  try {
    # Decks a and b come from this one template, as the decks of a team do.
    $t = $ppt.Presentations.Add(0)
    $t.SaveAs($template, 26)
    $t.Close()

    # A clip for deck c, made by PowerPoint itself.
    Step 'clip'
    $clip = [string](Join-Path $tmp 'clip.mp4')
    $v = $ppt.Presentations.Add(0)
    $v.PageSetup.SlideWidth = 160; $v.PageSetup.SlideHeight = 120
    $v.Slides.AddSlide(1, $v.SlideMaster.CustomLayouts.Item(7)) | Out-Null
    $v.CreateVideo($clip, $false, 1, 240, 10, 30)
    for ($i = 0; $i -lt 120 -and $v.CreateVideoStatus -lt 3; $i++) { Start-Sleep -Milliseconds 500 }
    if ($v.CreateVideoStatus -ne 3) { throw "The clip failed: status $($v.CreateVideoStatus)." }
    $v.Close()

    # a.pptx: 16:9, two sections, a chart with its workbook, notes on every slide, a modern
    # comment, a slide-jump link, a custom show with a button that runs it, a picture.
    Step 'a.pptx'
    $d = From-Template $ppt $template -1
    $ppt.WindowState = 2
    $s1 = Add-Slide $d 1 'Deck A'
    $s2 = Add-Slide $d 2 'Chart of A'
    $chart = Track @('EXCEL') { $s2.Shapes.AddChart2(-1, 51, 80, 120, 500, 320, $true) }
    try { $chart.Chart.ChartData.Workbook.Application.Quit() } catch {}
    $s3 = Add-Slide $d 2 'Detail of A'
    $s3.Shapes.Placeholders.Item(2).TextFrame.TextRange.Text = 'The target of the link.'
    $s4 = Add-Slide $d 6 'Picture of A'
    $s4.Shapes.AddPicture($png, 0, -1, 100, 150, 96, 96) | Out-Null
    Add-Jump $s1 $s3 'Go to the detail'
    $d.SlideShowSettings.NamedSlideShows.Add('Short', [int[]]@($s1.SlideID, $s3.SlideID)) | Out-Null
    Add-ShowButton $s4 'Short'
    $n = 0
    foreach ($s in $s1, $s2, $s3, $s4) { $n++; Add-Notes $s "Notes of A $n" }
    $s3.Comments.Add(12, 12, 'Author A', 'AA', 'Comment on A') | Out-Null
    $d.SectionProperties.AddBeforeSlide(1, 'Opening') | Out-Null
    $d.SectionProperties.AddBeforeSlide(3, 'Details') | Out-Null
    $d.SaveAs($saved.a, 24)
    $d.Close()

    # b.pptx: the same template, notes, a hidden slide, a legacy comment by Author A, the same
    # picture as a.pptx.
    Step 'b.pptx'
    $d = From-Template $ppt $template 0
    $s1 = Add-Slide $d 1 'Deck B'
    $s2 = Add-Slide $d 2 'Hidden slide of B'
    $s2.SlideShowTransition.Hidden = -1
    $s3 = Add-Slide $d 6 'Picture of B'
    $s3.Shapes.AddPicture($png, 0, -1, 200, 150, 96, 96) | Out-Null
    $n = 0
    foreach ($s in $s1, $s2, $s3) { $n++; Add-Notes $s "Notes of B $n" }
    $d.SaveAs($saved.b, 24)
    $d.Close()
    Add-LegacyComment $saved.b 'ppt/slides/slide3.xml'
    Save-Again $ppt $saved.b 0

    # c.pptx: 4:3, its own master and theme, a table, a video, a line in the embedded font, a
    # custom show named like a.pptx's with a button, a slide-jump link.
    Step 'c.pptx'
    $d = $ppt.Presentations.Add(0)
    $d.PageSetup.SlideWidth = 720; $d.PageSetup.SlideHeight = 540
    $d.SlideMaster.Name = 'Deck C design'
    $d.SlideMaster.Theme.ThemeColorScheme.Colors(5).RGB = 0x2F7F2F
    $d.SlideMaster.Theme.ThemeColorScheme.Colors(6).RGB = 0x7F2F7F
    $d.SlideMaster.Theme.ThemeFontScheme.MajorFont.Item(1).Name = 'Merge Box'
    $d.SlideMaster.Shapes.AddShape(1, 20, 500, 680, 20).Name = 'Band'
    $s1 = Add-Slide $d 1 'Deck C'
    $s2 = Add-Slide $d 6 'Table of C'
    $s2.Shapes.AddTable(2, 3, 60, 140, 600, 120).Name = 'Styled table'
    $s3 = Add-Slide $d 6 'Video of C'
    $s3.Shapes.AddMediaObject2($clip, 0, -1, 120, 140, 240, 180) | Out-Null
    $line = Add-Text $s3 'Boxed text' 60 440
    $line.TextFrame.TextRange.Font.Name = 'Merge Box'
    Add-Jump $s1 $s3 'Go to the video'
    $d.SlideShowSettings.NamedSlideShows.Add('Short', [int[]]@($s2.SlideID)) | Out-Null
    Add-ShowButton $s2 'Short'
    Add-Notes $s1 'Notes of C 1'
    $d.SaveAs($saved.c, 24, -1)
    $d.Close()

    # COM can't author a custom table style: patch it in, then let PowerPoint save the file.
    Step 'c.pptx table style'
    $styleId = '{0D1F6A3B-5C1E-4E1B-9C55-3A7C2B6E1F10}'
    $style = "<a:tblStyle styleId=""$styleId"" styleName=""Deck C table""><a:wholeTbl><a:tcTxStyle><a:fontRef idx=""minor""/><a:schemeClr val=""dk1""/></a:tcTxStyle><a:tcStyle><a:tcBdr/><a:fill><a:solidFill><a:srgbClr val=""FFE699""/></a:solidFill></a:fill></a:tcStyle></a:wholeTbl></a:tblStyle>"
    Edit-Package $saved.c {
      param($name, $text)
      if ($name -eq 'ppt/tableStyles.xml') {
        if ($text -match '/>\s*$') { return ($text -replace '/>\s*$', ">$style</a:tblStyleLst>") }
        return $text -replace '</a:tblStyleLst>', "$style</a:tblStyleLst>"
      }
      if ($name -match '^ppt/slides/slide\d+\.xml$') { return $text -replace '<a:tableStyleId>[^<]*</a:tableStyleId>', "<a:tableStyleId>$styleId</a:tableStyleId>" }
    }.GetNewClosure()
    Save-Again $ppt $saved.c -1
  } finally {
    try { $ppt.Quit() } catch {}
    Start-Sleep -Seconds 2
    Stop-Mine
    [Native.Gdi]::RemoveFontResourceW($ttf) | Out-Null
  }

  foreach ($n in 'a', 'b', 'c') {
    Edit-Package $saved[$n] { param($name, $text) Scrub $name $text 'Author A' }.GetNewClosure()
  }
}

try {
  switch ($Family) { 'word' { Word-Docs } 'excel' { Excel-Books } 'powerpoint' { Ppt-Decks } }
} catch {
  $_.InvocationInfo.PositionMessage
  throw
} finally {
  Stop-Mine
  Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}
$files = @{ word = '*.docx'; excel = '*.xlsx'; powerpoint = '*.pptx' }[$Family]
Get-ChildItem $out -Filter $files | ForEach-Object { "{0} {1:N0} bytes" -f $_.Name, $_.Length }
