use std::num::NonZeroUsize;

use ecow::eco_format;
use pdf_writer::{
    types::Direction, writers::PageLabel, Finish, Name, Pdf, Ref, Str, TextStr,
};
use xmp_writer::{DateTime, LangId, RenditionClass, Timezone, XmpWriter};

use typst::foundations::{Datetime, Smart};
use typst::layout::Dir;
use typst::text::Lang;

use crate::WithEverything;
use crate::{hash_base64, outline, page::PdfPageLabel};

pub struct PdfSig {
    pub name: String,
    pub location: String,
    pub reason: String,
    pub contact_info: String,
}

/// Write the document catalog.
pub fn write_catalog(
    ctx: WithEverything,
    ident: Smart<&str>,
    timestamp: Option<Datetime>,
    pdf: &mut Pdf,
    alloc: &mut Ref,
    signer: Option<PdfSig>,
) {
    let lang = ctx
        .resources
        .languages
        .iter()
        .max_by_key(|(_, &count)| count)
        .map(|(&l, _)| l);

    let dir = if lang.map(Lang::dir) == Some(Dir::RTL) {
        Direction::R2L
    } else {
        Direction::L2R
    };

    // Write the outline tree.
    let outline_root_id = outline::write_outline(pdf, alloc, &ctx);

    // Write the page labels.
    let page_labels = write_page_labels(pdf, alloc, &ctx);

    // Write the document information.
    let info_ref = alloc.bump();
    let mut info = pdf.document_info(info_ref);
    let mut xmp = XmpWriter::new();
    if let Some(title) = &ctx.document.title {
        info.title(TextStr(title));
        xmp.title([(None, title.as_str())]);
    }

    let authors = &ctx.document.author;
    if !authors.is_empty() {
        // Turns out that if the authors are given in both the document
        // information dictionary and the XMP metadata, Acrobat takes a little
        // bit of both: The first author from the document information
        // dictionary and the remaining authors from the XMP metadata.
        //
        // To fix this for Acrobat, we could omit the remaining authors or all
        // metadata from the document information catalog (it is optional) and
        // only write XMP. However, not all other tools (including Apple
        // Preview) read the XMP data. This means we do want to include all
        // authors in the document information dictionary.
        //
        // Thus, the only alternative is to fold all authors into a single
        // `<rdf:li>` in the XMP metadata. This is, in fact, exactly what the
        // PDF/A spec Part 1 section 6.7.3 has to say about the matter. It's a
        // bit weird to not use the array (and it makes Acrobat show the author
        // list in quotes), but there's not much we can do about that.
        let joined = authors.join(", ");
        info.author(TextStr(&joined));
        xmp.creator([joined.as_str()]);
    }

    let creator = eco_format!("Typst {}", env!("CARGO_PKG_VERSION"));
    info.creator(TextStr(&creator));
    xmp.creator_tool(&creator);

    let keywords = &ctx.document.keywords;
    if !keywords.is_empty() {
        let joined = keywords.join(", ");
        info.keywords(TextStr(&joined));
        xmp.pdf_keywords(&joined);
    }

    let create_date = if let Some(date) = ctx.document.date.unwrap_or(timestamp) {
        let tz = ctx.document.date.is_auto();
        let pdf_date_opt = pdf_date(date, tz);
        if let Some(pdf_date) = pdf_date_opt {
            info.creation_date(pdf_date);
            info.modified_date(pdf_date);
        }
        if let Some(xmp_date) = xmp_date(date, tz) {
            xmp.create_date(xmp_date);
            xmp.modify_date(xmp_date);
        }
        pdf_date_opt
    } else {None};

    info.finish();
    xmp.num_pages(ctx.document.pages.len() as u32);
    xmp.format("application/pdf");
    xmp.language(ctx.resources.languages.keys().map(|lang| LangId(lang.as_str())));

    // A unique ID for this instance of the document. Changes if anything
    // changes in the frames.
    let instance_id = hash_base64(&pdf.as_bytes());

    // Determine the document's ID. It should be as stable as possible.
    const PDF_VERSION: &str = "PDF-1.7";
    let doc_id = if let Smart::Custom(ident) = ident {
        // We were provided with a stable ID. Yay!
        hash_base64(&(PDF_VERSION, ident))
    } else if ctx.document.title.is_some() && !ctx.document.author.is_empty() {
        // If not provided from the outside, but title and author were given, we
        // compute a hash of them, which should be reasonably stable and unique.
        hash_base64(&(PDF_VERSION, &ctx.document.title, &ctx.document.author))
    } else {
        // The user provided no usable metadata which we can use as an `/ID`.
        instance_id.clone()
    };

    // Write IDs.
    xmp.document_id(&doc_id);
    xmp.instance_id(&instance_id);
    pdf.set_file_id((doc_id.clone().into_bytes(), instance_id.into_bytes()));

    xmp.rendition_class(RenditionClass::Proof);
    xmp.pdf_version("1.7");

    let xmp_buf = xmp.finish(None);
    let meta_ref = alloc.bump();
    pdf.stream(meta_ref, xmp_buf.as_bytes())
        .pair(Name(b"Type"), Name(b"Metadata"))
        .pair(Name(b"Subtype"), Name(b"XML"));

    // Write the document catalog.
    let catalog_ref = alloc.bump();
    let mut catalog = pdf.catalog(catalog_ref);
    catalog.pages(ctx.page_tree_ref);
    catalog.viewer_preferences().direction(dir);
    catalog.metadata(meta_ref);

    // Write the named destination tree.
    let mut name_dict = catalog.names();
    let mut dests_name_tree = name_dict.destinations();
    let mut names = dests_name_tree.names();
    for &(name, dest_ref, ..) in &ctx.references.named_destinations.dests {
        names.insert(Str(name.as_str().as_bytes()), dest_ref);
    }
    names.finish();
    dests_name_tree.finish();
    name_dict.finish();

    // Insert the page labels.
    if !page_labels.is_empty() {
        let mut num_tree = catalog.page_labels();
        let mut entries = num_tree.nums();
        for (n, r) in &page_labels {
            entries.insert(n.get() as i32 - 1, *r);
        }
    }

    if let Some(outline_root_id) = outline_root_id {
        catalog.outlines(outline_root_id);
    }

    if let Some(lang) = lang {
        catalog.lang(TextStr(lang.as_str()));
    }

    // this SIG need post processing
    // - fill sig_content with hash of pdf-finished bytes before and after sig_content  
    // - change ByteRange to match actual sig position
    if let (Some(sig), Some(date_pdf)) = (signer, create_date) {
        if let Some(Some(first_page_id)) = ctx.globals.pages.iter().find(|page| page.is_some()) {
            
            let widget_id = alloc.bump();
            let sig_id = alloc.bump();

            let mut sig_contents = [0u8;14443];
            sig_contents[0] = 40;
            sig_contents[1] = 41;

            catalog.insert(Name(b"Perms")).dict().pair(Name(b"DocMDP"), sig_id);
    
            let mut acro_form = catalog.insert(Name(b"AcroForm")).dict();
            acro_form.pair(Name(b"SigFlags"), 3)
                .insert(Name(b"Fields")).array().item(widget_id);
            acro_form.finish();
            catalog.finish();
    
            pdf.indirect(widget_id).dict()
                .pair(Name(b"F"), 130)
                .pair(Name(b"Type"), Name(b"Annot"))
                .pair(Name(b"SubType"), Name(b"Widget"))
                .pair(Name(b"Rect"), pdf_writer::Rect::new(0.0, 0.0, 0.0, 0.0))
                .pair(Name(b"FT"), Name(b"Sig"))
                .pair(Name(b"V"), sig_id)
                .pair(Name(b"T"), TextStr("Signature"))
                .pair(Name(b"P"), first_page_id);
    
            pdf.indirect(sig_id).dict()
                .pair(Name(b"Type"), Name(b"Sig"))
                .pair(Name(b"Filter"), Name(b"Adobe.PPKLite"))
                .pair(Name(b"SubFilter"), Name(b"adbe.pkcs7.detached"))
                .pair(Name(b"M"), date_pdf) 
                .pair(Name(b"Name"), TextStr(sig.name.as_str()))
                .pair(Name(b"Location"), TextStr(sig.location.as_str()))
                .pair(Name(b"Reason"), TextStr(sig.reason.as_str()))
                .pair(Name(b"ContactInfo"), TextStr(sig.contact_info.as_str()))
                .pair(Name(b"Contents"), Str(&sig_contents))
                .pair(Name(b"ByteRange"), pdf_writer::Rect::new(88888888.0, 88888888.0, 88888888.0, 88888888.0))
                .insert(Name(b"Reference")).array().push().dict()
                    .pair(Name(b"Type"), Name(b"SigRef"))
                    .pair(Name(b"Data"), catalog_ref)
                    .pair(Name(b"TransformMethod"), Name(b"DocMDP"))
                    .insert(Name(b"TransformParams")).dict()
                        .pair(Name(b"Type"), Name(b"TransformParams"))
                        .pair(Name(b"V"), Name(b"1.2"))
                        .pair(Name(b"P"), 1);
        }
    }
}

/// Write the page labels.
pub(crate) fn write_page_labels(
    chunk: &mut Pdf,
    alloc: &mut Ref,
    ctx: &WithEverything,
) -> Vec<(NonZeroUsize, Ref)> {
    // If there is no exported page labeled, we skip the writing
    if !ctx.pages.iter().filter_map(Option::as_ref).any(|p| {
        p.label
            .as_ref()
            .is_some_and(|l| l.prefix.is_some() || l.style.is_some())
    }) {
        return Vec::new();
    }

    let mut result = vec![];
    let empty_label = PdfPageLabel::default();
    let mut prev: Option<&PdfPageLabel> = None;

    // Skip non-exported pages for numbering.
    for (i, page) in ctx.pages.iter().filter_map(Option::as_ref).enumerate() {
        let nr = NonZeroUsize::new(1 + i).unwrap();
        // If there are pages with empty labels between labeled pages, we must
        // write empty PageLabel entries.
        let label = page.label.as_ref().unwrap_or(&empty_label);

        if let Some(pre) = prev {
            if label.prefix == pre.prefix
                && label.style == pre.style
                && label.offset == pre.offset.map(|n| n.saturating_add(1))
            {
                prev = Some(label);
                continue;
            }
        }

        let id = alloc.bump();
        let mut entry = chunk.indirect(id).start::<PageLabel>();

        // Only add what is actually provided. Don't add empty prefix string if
        // it wasn't given for example.
        if let Some(prefix) = &label.prefix {
            entry.prefix(TextStr(prefix));
        }

        if let Some(style) = label.style {
            entry.style(style.to_pdf_numbering_style());
        }

        if let Some(offset) = label.offset {
            entry.offset(offset.get() as i32);
        }

        result.push((nr, id));
        prev = Some(label);
    }

    result
}

/// Converts a datetime to a pdf-writer date.
fn pdf_date(datetime: Datetime, tz: bool) -> Option<pdf_writer::Date> {
    let year = datetime.year().filter(|&y| y >= 0)? as u16;

    let mut pdf_date = pdf_writer::Date::new(year);

    if let Some(month) = datetime.month() {
        pdf_date = pdf_date.month(month);
    }

    if let Some(day) = datetime.day() {
        pdf_date = pdf_date.day(day);
    }

    if let Some(h) = datetime.hour() {
        pdf_date = pdf_date.hour(h);
    }

    if let Some(m) = datetime.minute() {
        pdf_date = pdf_date.minute(m);
    }

    if let Some(s) = datetime.second() {
        pdf_date = pdf_date.second(s);
    }

    if tz {
        pdf_date = pdf_date.utc_offset_hour(0).utc_offset_minute(0);
    }

    Some(pdf_date)
}

/// Converts a datetime to an xmp-writer datetime.
fn xmp_date(datetime: Datetime, tz: bool) -> Option<xmp_writer::DateTime> {
    let year = datetime.year().filter(|&y| y >= 0)? as u16;
    Some(DateTime {
        year,
        month: datetime.month(),
        day: datetime.day(),
        hour: datetime.hour(),
        minute: datetime.minute(),
        second: datetime.second(),
        timezone: if tz { Some(Timezone::Utc) } else { None },
    })
}
