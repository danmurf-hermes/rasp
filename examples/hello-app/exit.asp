<%@ Language=VBScript %>
<%
' Exit flows and conversions on one page.
out = ""
For i = 1 To 100
  If i = 4 Then Exit For
  out = out & i
Next
n = 0
Do While True
  n = n + 1
  If n >= 3 Then Exit Do
Loop
d = CDate("9/30/2026")
%>
<p>loop1=<%= out %></p>
<p>loop2=<%= n %></p>
<p>round=<%= CInt(2.5) & CInt(3.5) %></p>
<p>date=<%= Year(d) & "-" & Month(d) & "-" & Day(d) %></p>
<p>eom=<%= DateSerial(2026, 2, 29) %></p>