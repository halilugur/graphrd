package com.example.personnel.repository;

import java.util.List;

import com.example.personnel.model.entity.SalaryReview;
import com.graphrd.springdata.GraphRdRepository;

/** The salary-review records, as a plain Spring Data repository. */
public interface ReviewsRepository extends GraphRdRepository<SalaryReview, String> {

    /** The raise history of one employee, newest first — a reference condition. */
    List<SalaryReview> findByEmployeeOrderByDateDesc(String employeeIri);
}
