"""Recognising a permissive licence from its text, for `--translate-libs` (see libsrc.py).

A distribution whose metadata names its licence in free text (`License: BSD`, `BSD-like`) says nothing a machine can
rely on. Its licence file can: the text is compared, word for word, with the canonical licences (MIT, BSD-2/3/4-Clause,
ISC, 0BSD, Zlib, HPND, Apache-2.0, Unlicense). Only the holder's names and the copyright lines vary; every other word
must match, so a modified licence matches nothing. Text left over once the recognised licences are removed must
contain no grant, restriction or foreign licence (the `unexplained` result); anything doubtful is refused.
"""

import hashlib
import re

# the licences a text can be recognised as; libsrc.ADMITTED holds those admitted by default
_W = r"[a-z0-9]+"
_NAME = r"[a-z0-9 ]{1,120}?"  # a holder, project or product name (bounded, so a rider cannot hide in it)

_BSD_HEAD = ("redistribution and use in source and binary forms with or without modification are permitted provided "
             "that the following conditions are met ")
_BSD_C1 = ("(?:\\d+ )?redistributions of source code must retain the above copyright notice this list of conditions "
           "and the following disclaimer ")
_BSD_C2 = ("(?:\\d+ )?redistributions in binary form must reproduce the above copyright notice this list of conditions "
           "and the following disclaimer in the documentation and or other materials provided with the distribution ")
_BSD_C3 = ("(?:\\d+ )?(?:neither|none of) " + _NAME + " may be used to endorse or promote products derived from this "
           "software without specific prior written permission ")
_BSD_ADV = ("(?:\\d+ )?all advertising materials mentioning features or use of this software must display the "
            "following acknowledg(?:e)?ment this product includes software developed by " + _NAME + " ")
_BSD_ACK = ("(?:\\d+ )?redistributions of any form whatsoever must retain the following acknowledg(?:e)?ment this "
            "product includes software developed by " + _NAME + " ")
_BSD_DISC = ("this software is provided by " + _NAME + " as is and any express(?:ed)? or implied warranties including but not "
             "limited to the implied warranties of merchantability and fitness for a particular purpose are disclaimed "
             "in no event shall " + _NAME + " be liable for any direct indirect incidental special exemplary or "
             "consequential damages including but not limited to procurement of substitute goods or services loss of "
             "use data or profits or business interruption however caused and on any theory of liability whether in "
             "contract strict liability or tort including negligence or otherwise arising in any way out of the use "
             "of this software even if advised of the possibility of such damage")

_ISC_GRANT = ("permission to use copy modify and or distribute this software for any purpose with or without fee is "
              "hereby granted ")
_ISC_COND = "provided that the above copyright notice and this permission notice appear in all copies "
_ISC_DISC = ("the software is provided as is and the authors? disclaims? all warranties with regard to this software "
             "including all implied warranties of merchantability and fitness in no event shall the authors? be "
             "liable for any (?:special direct indirect|direct indirect special) or consequential damages or any "
             "damages whatsoever resulting from loss of use data or profits whether in an action of contract "
             "negligence or other tort?(?:i)?ous action arising out of or in connection with the use or performance "
             "of this software")

_MIT = ("permission is hereby granted free of charge to any person obtaining a copy of this software and associated "
        "documentation files the software to deal in the software without restriction including without limitation "
        "the rights to use copy modify merge publish distribute sublicense and or sell copies of the software and to "
        "permit persons to whom the software is furnished to do so subject to the following conditions the above "
        "copyright notice and this permission notice shall be included in all copies or substantial portions of the "
        "software the software is provided as is without warranty of any kind express or implied including but not "
        "limited to the warranties of merchantability fitness for a particular purpose and noninfringement in no "
        "event shall " + _NAME + " be liable for any claim damages or other liability whether in an action of "
        "contract tort or otherwise arising from out of or in connection with the software or the use or other "
        "dealings in the software")

_ZLIB = ("this software is provided as is without any express or implied warranty in no event will the authors? be "
         "held liable for any damages arising from the use of this software permission is granted to anyone to use "
         "this software for any purpose including commercial applications and to alter it and redistribute it "
         "freely subject to the following restrictions 1 the origin of this software must not be misrepresented you "
         "must not claim that you wrote the original software if you use this software in a product an "
         "acknowledg(?:e)?ment in the product documentation would be appreciated but is not required 2 altered "
         "source versions must be plainly marked as such and must not be misrepresented as being the original "
         "software 3 this notice may not be removed or altered from any source distribution")

_HPND = ("(?:by obtaining using and or copying this software and or its associated documentation you agree that you "
         "have read understood and will comply with the following terms and conditions )?"
         "permission to use copy modify and distribute this software and its associated documentation for any "
         "purpose and without fee is hereby granted provided that the above copyright notice appears in all copies "
         "and that both that copyright notice and this permission notice appear in supporting documentation and that "
         "the name of " + _NAME + " not be used in advertising or publicity pertaining to distribution of the "
         "software without specific written prior permission " + _NAME + " disclaims all warranties with regard to "
         "this software including all implied warranties of merchantability and fitness in no event shall " + _NAME +
         " be liable for any special indirect or consequential damages or any damages whatsoever resulting from loss "
         "of use data or profits whether in an action of contract negligence or other tortious action arising out of "
         "or in connection with the use or performance of this software")

_UNLICENSE = ("this is free and unencumbered software released into the public domain anyone is free to copy modify "
              "publish use compile sell or distribute this software either in source code form or as a compiled "
              "binary for any purpose commercial or non commercial and by any means in jurisdictions that recognize "
              "copyright laws the author or authors of this software dedicate any and all copyright interest in the "
              "software to the public domain we make this dedication for the benefit of the public at large and to "
              "the detriment of our heirs and successors we intend this dedication to be an overt act of "
              "relinquishment in perpetuity of all present and future rights to this software under copyright law "
              "the software is provided as is without warranty of any kind express or implied including but not "
              "limited to the warranties of merchantability fitness for a particular purpose and noninfringement in "
              "no event shall the authors be liable for any claim damages or other liability whether in an action of "
              "contract tort or otherwise arising from out of or in connection with the software or the use or other "
              "dealings in the software for more information please refer to")

# SHA-256 of the word-normalised Apache License 2.0, from "TERMS AND CONDITIONS" to "END OF TERMS AND CONDITIONS"
# (the appendix and the copyright line are not part of it): a single changed word changes it
_APACHE_SHA256 = "d8221a4f10c01c6fa5390685929a5ce31d7264a024162fd3dd4d805c57ce0f43"

# order matters: a longer licence before the shorter one that is its prefix (BSD-4 and 3 before 2)
_BLOCKS = (
    ("BSD-4-Clause", _BSD_HEAD + _BSD_C1 + _BSD_C2 + "(?:" + _BSD_C3 + ")?" + _BSD_ADV + "(?:" + _BSD_C3 + ")?(?:"
     + _BSD_ACK + ")?" + _BSD_DISC),
    ("BSD-3-Clause", _BSD_HEAD + _BSD_C1 + _BSD_C2 + _BSD_C3 + _BSD_DISC),
    ("BSD-2-Clause", _BSD_HEAD + _BSD_C1 + _BSD_C2 + _BSD_DISC),
    ("MIT", _MIT),
    ("ISC", _ISC_GRANT + _ISC_COND + _ISC_DISC),
    ("0BSD", _ISC_GRANT + _ISC_DISC),
    ("Zlib", _ZLIB),
    ("HPND", _HPND),
    ("Unlicense", _UNLICENSE),
)
_COMPILED = tuple((spdx, re.compile(rx)) for spdx, rx in _BLOCKS)

# what the leftover text may say: names of admitted licences (`licensed under the MIT license`) and copyright lines
_ADMITTED_NAMES = re.compile(
    r"(?:under|is|are|as) (?:the |a )?(?:mit|bsd|isc|apache|zlib|python|psf|new bsd|simplified bsd|3 clause bsd|"
    r"2 clause bsd)(?: [a-z0-9]+){0,3}? licen[cs]es?")
# words that make leftover text a grant, a restriction or a licence of its own
_FOREIGN = re.compile(
    r"\b(?:permission|redistribut|hereby|granted|without fee|sublicen|royalt|patent|warrant|liab|"
    r"non ?commercial|commercial use|prohibit|forbidden|not be used|may not|shall not|must not|"
    r"creative commons|general public|gnu|lgpl|gpl|mozilla|mpl|eclipse|copyleft|proprietary|beer|"
    r"public domain|all rights reserved to|advertis)")
_MAX_LEFT = 150  # words of leftover text beyond which a licence file is not just names and copyright lines


def words(text: str) -> str:
    """Lower-case words only: comment marks, bullets, quotes, markup and line breaks are not the licence."""
    return " ".join(re.findall(_W, text.lower().replace("&", " and ").replace("/", " ")))


def apache_hash(text: str) -> str | None:
    w = words(text)
    i = w.find("terms and conditions for use reproduction and distribution")
    j = w.find("end of terms and conditions")
    if i < 0 or j < i:
        return None
    return hashlib.sha256(w[i:j + len("end of terms and conditions")].encode()).hexdigest()


def recognise(text: str) -> tuple[list[str], str]:
    """The licences `text` consists of and what is left of it: ([SPDX ids], unexplained excerpt or "").

    The excerpt is empty when every word of the file belongs to a recognised licence, to a copyright line or to a
    mention of an admitted licence by name. A non-empty excerpt is the first thing the file says that py2axum does
    not recognise (a refusal quotes it)."""
    w = " " + words(text) + " "
    found: list[str] = []
    h = apache_hash(text)
    if h is not None and h == _APACHE_SHA256:
        i = w.find("terms and conditions for use reproduction and distribution")
        j = w.find("end of terms and conditions") + len("end of terms and conditions")
        found.append("Apache-2.0")
        w = w[:i] + " " + w[j:]
        # the title and the appendix are Apache's own: the leftover starts after them
        w = re.sub(r"apache license version 2 0 january 2004 https? www apache org licenses", " ", w)
        w = re.sub(r"appendix how to apply the apache license to your work.*?under the license ", " ", w)
    for spdx, rx in _COMPILED:
        m = rx.search(w.strip())
        while m:
            found.append(spdx)
            s = w.strip()
            w = " " + s[:m.start()] + " " + s[m.end():] + " "
            m = rx.search(w.strip())
    left = w
    left = _ADMITTED_NAMES.sub(" ", left)
    # copyright lines: "copyright (c) 2008 Greg Hewgill http://hewgill.com", "all rights reserved"
    left = re.sub(r"all rights reserved", " ", left)
    left = re.sub(r"(?:portions )?copyright(?: c)?(?: \d{4}(?: \d{4})?)?(?: by)?(?: [a-z0-9]+){0,12}?"
                  r"(?= copyright| licen| the | this | see | all | redistrib| permission| $)", " ", left)
    left = " ".join(left.split())
    if not found and not left:
        return found, ""
    m = _FOREIGN.search(left)
    if m or len(left.split()) > _MAX_LEFT:
        at = m.start() if m else 0
        ctx = left[max(0, at - 60):at + 140]
        if at > 60:
            ctx = ctx.split(" ", 1)[-1]  # start on a whole word
        return list(dict.fromkeys(found)), ctx.strip()
    return list(dict.fromkeys(found)), ""
