package com.example.personnel.controller;

import java.util.List;

import org.springframework.web.bind.annotation.GetMapping;
import org.springframework.web.bind.annotation.RequestMapping;
import org.springframework.web.bind.annotation.RequestParam;
import org.springframework.web.bind.annotation.RestController;

import com.example.personnel.model.dto.PayrollRow;
import com.example.personnel.model.dto.Totals;
import com.example.personnel.service.PayrollService;

import lombok.RequiredArgsConstructor;

/** The numbers, all computed by the server in SPARQL. */
@RestController
@RequestMapping("/reports")
@RequiredArgsConstructor
class ReportController {

    private final PayrollService payroll;

    /** Per company: headcount, payroll, average, lowest, highest. */
    @GetMapping("/payroll")
    List<PayrollRow> payroll() {
        return payroll.payroll();
    }

    /** One row for the whole organization. */
    @GetMapping("/totals")
    Totals totals() {
        return payroll.totals();
    }

    /** What the raises on record have cost so far — or only since a date. */
    @GetMapping("/raise-cost")
    long raiseCost(@RequestParam(required = false) String since) {
        return since == null || since.isBlank() ? payroll.raiseCost() : payroll.raiseCostSince(since);
    }

    /** One company's row, by name: {@code /reports/payroll/company?name=Acme}. */
    @GetMapping("/payroll/company")
    PayrollRow companyPayroll(@RequestParam String name) {
        return payroll.companyPayroll(name);
    }
}
